#![feature(slice_concat_ext)]

extern crate slog;
#[macro_use]
extern crate serde_derive;
extern crate toml;
extern crate distributary;
extern crate mysql;

use mysql::OptsBuilder;
use mysql::value::Params;

use std::path::Path;
use std::io::{Read, Write};
use std::fs::{self, File};
use std::collections::{BTreeMap, HashSet};
use std::slice::SliceConcatExt;
use std::str::FromStr;

use std::fmt::Write as FmtWrite;

use std::thread;
use std::time;
use std::io;

use distributary::{Blender, Recipe, DataType};

const DIRECTORY_PREFIX: &str = "tests/mysql_comparison_tests";

#[derive(Debug, Deserialize)]
enum Type {
    Int,
    Text,
}

impl Type {
    pub fn make_datatype(&self, value: &str) -> DataType {
        match *self {
            Type::Int => i64::from_str(value).unwrap().into(),
            Type::Text => value.into(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct Table {
    create_query: String,
    types: Vec<Type>,
    data: Vec<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct Query {
    select_query: String,
    types: Vec<Type>,
    values: Vec<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct Schema {
    name: String,
    tables: BTreeMap<String, Table>,
    queries: BTreeMap<String, Query>,
}

fn read_file<P: AsRef<Path>>(file_name: P) -> String {
    let mut contents = String::new();
    let mut file = File::open(file_name).unwrap();
    file.read_to_string(&mut contents).unwrap();
    contents
}

fn write_file<P: AsRef<Path>>(file_name: P, contents: String) {
    let mut file = File::create(file_name).unwrap();
    file.write_all(contents.as_bytes()).unwrap();
}

fn run_for_all_in_directory<F: FnMut(String, String)>(directory: &str, mut f: F) {
    let directory = Path::new(DIRECTORY_PREFIX).join(directory);
    for entry in fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        f(
            entry.file_name().to_str().unwrap().to_owned(),
            read_file(entry.path().to_str().unwrap()),
        );
    }
}

pub fn setup_mysql(addr: &str) -> mysql::Pool {
    use mysql::Opts;

    let addr = format!("mysql://{}", addr);
    let db = &addr[addr.rfind("/").unwrap() + 1..];
    let options = Opts::from_url(&addr[0..addr.rfind("/").unwrap()]).unwrap();

    // clear the db (note that we strip of /db so we get default)
    let mut opts = OptsBuilder::from_opts(options.clone());
    opts.db_name(Some(db));
    opts.init(vec!["SET max_heap_table_size = 4294967296;"]);
    let pool = mysql::Pool::new_manual(1, 4, opts).unwrap();
    let mut conn = pool.get_conn().unwrap();
    if conn.query(format!("USE {}", db)).is_ok() {
        conn.query(format!("DROP DATABASE {}", &db).as_str())
            .unwrap();
    }
    conn.query(format!("CREATE DATABASE {}", &db).as_str())
        .unwrap();
    conn.query(format!("USE {}", db)).unwrap();

    drop(conn);

    // now we connect for real
    let mut opts = OptsBuilder::from_opts(options);
    opts.db_name(Some(db));
    opts.init(vec!["SET max_heap_table_size = 4294967296;"]);
    mysql::Pool::new_manual(1, 4, opts).unwrap()
}

fn generate_target_results(schemas: &BTreeMap<String, Schema>) {
    for (schema_name, schema) in schemas.iter() {
        let pool = setup_mysql("soup:password@127.0.0.1:3306/mysql_comparison_test");
        for (table_name, table) in schema.tables.iter() {
            pool.prep_exec(&table.create_query, ()).unwrap();
            for row in table.data.iter() {
                let row: Vec<_> = row.iter()
                    .zip(table.types.iter())
                    .map(|(v, t)| match *t {
                        Type::Text => format!("\"{}\"", v),
                        Type::Int => v.clone(),
                    })
                    .collect();
                let insert_query =
                    format!("INSERT INTO {} VALUES ({})", table_name, row.join(", "));
                pool.prep_exec(&insert_query, ()).unwrap();
            }
        }

        let mut target_data: BTreeMap<String, BTreeMap<String, Vec<Vec<String>>>> = BTreeMap::new();
        for (query_name, query) in schema.queries.iter() {
            target_data.insert(query_name.clone(), BTreeMap::new());

            for (i, values) in query.values.iter().enumerate() {
                target_data.get_mut(query_name).unwrap().insert(
                    i.to_string(),
                    Vec::new(),
                );

                let values = Params::Positional(values.iter().map(|v| v.into()).collect());
                for row in pool.prep_exec(&query.select_query, values).unwrap() {
                    let row = row.unwrap()
                        .unwrap()
                        .into_iter()
                        .map(|v| {
                            v.into_str()
                                .trim_matches(|c| c == '\'' || c == '"')
                                .to_owned()
                        })
                        .collect();
                    target_data
                        .get_mut(query_name)
                        .unwrap()
                        .get_mut(&i.to_string())
                        .unwrap()
                        .push(row);
                }
            }
        }
        let target_data_toml = toml::to_string(&target_data).unwrap();
        let target_data_file = Path::new(DIRECTORY_PREFIX).join("targets").join(
            schema_name,
        );
        write_file(target_data_file, target_data_toml);
    }
}

fn check_query(
    tables: &BTreeMap<String, Table>,
    query_name: &str,
    query: &Query,
    target: &BTreeMap<String, Vec<Vec<String>>>,
) -> Result<(), String> {
    let mut error_log = String::new();
    let mut g = Blender::new();
    g.disable_sharding();
    let recipe;
    {
        // migrate
        let mut mig = g.start_migration();

        let queries: Vec<_> = tables
            .values()
            .map(|t| t.create_query.clone())
            .chain(Some(query_name.to_owned() + ": " + &query.select_query))
            .collect();

        recipe = match Recipe::from_str(&queries.join("\n"), None) {
            Ok(mut recipe) => {
                recipe.activate(&mut mig, false).unwrap();
                recipe
            }
            Err(e) => panic!(e),
        };

        mig.commit();
    }

    for (table_name, table) in tables.iter() {
        let mut mutator = g.get_mutator(recipe.node_addr_for(table_name).unwrap());
        for row in table.data.iter() {
            assert_eq!(row.len(), table.types.len());
            let row: Vec<DataType> = row.iter()
                .enumerate()
                .map(|(i, v)| table.types[i].make_datatype(v))
                .collect();
            mutator.put(row).unwrap();
        }
    }

    thread::sleep(time::Duration::from_millis(300));

    let nd = recipe.node_addr_for(query_name).unwrap();
    let getter = g.get_getter(nd).unwrap();

    for (i, query_parameter) in query.values.iter().enumerate() {
        let query_parameter = query.types[0].make_datatype(&query_parameter[0]);
        let query_results = getter(&query_parameter, true).unwrap();

        let target_results = &target[&i.to_string()];
        let mut query_results: HashSet<Vec<String>> = query_results
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|v| match v {
                        DataType::BigInt(i) => i.to_string(),
                        DataType::Text(_) |
                        DataType::TinyText(_) => v.into(),
                        _ => unimplemented!(),

                    })
                    .collect()
            })
            .collect();

        writeln!(&mut error_log, "query_results").unwrap();
        for r in query_results.iter() {
            writeln!(&mut error_log, "{:?}", r).unwrap();
        }
        writeln!(&mut error_log, "\ntarget_results").unwrap();
        for r in target_results.iter() {
            writeln!(&mut error_log, "{:?}", r).unwrap();
        }
        writeln!(&mut error_log, "").unwrap();

        if query_results.len() != target_results.len() {
            writeln!(
                &mut error_log,
                "Wrong number of results (expected {}, got {})",
                target_results.len(),
                query_results.len()
            ).unwrap();

            return Err(error_log);
        }
        for target_row in target_results.iter() {
            if !query_results.remove(target_row) {
                writeln!(
                    &mut error_log,
                    "query_results and target_results do not match"
                ).unwrap();
                return Err(error_log);
            }
        }
    }
    Ok(())
}

#[test]
fn mysql_comparison() {
    let mut schemas: BTreeMap<String, Schema> = BTreeMap::new();
    run_for_all_in_directory("schemas", |file_name, contents| {
        {
            let ext = Path::new(&file_name).extension();
            if ext.is_none() || ext.unwrap() != "toml" {
                return;
            }
        }
        schemas.insert(file_name, toml::from_str(&contents).unwrap());
    });

    if cfg!(feature = "generate_mysql_tests") {
        generate_target_results(&schemas);
    }

    for (schema_name, schema) in schemas.iter() {
        let target_data_file = Path::new(DIRECTORY_PREFIX).join("targets").join(
            schema_name,
        );
        let target_data: BTreeMap<String, BTreeMap<String, Vec<Vec<String>>>> =
            toml::from_str(&read_file(target_data_file)).unwrap();

        for (query_name, query) in schema.queries.iter() {
            print!("{}.{}... ", schema.name, query_name);
            io::stdout().flush().ok().expect("Could not flush stdout");
            match check_query(&schema.tables, query_name, query, &target_data[query_name]) {
                Ok(()) => println!("\x1B[32;1mPASS\x1B[m"),
                Err(e) => println!("\x1B[31;1mFAIL\x1B[m:\n{}", e),
            }
        }
    }
}
