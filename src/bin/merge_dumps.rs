use std::{fs::OpenOptions, io::Write};

use convert_invert::internals::search::threaded_search::DumpData;
fn main() {
    let dumps: Vec<_> = std::fs::read_dir(".")
        .unwrap()
        .into_iter()
        .flatten()
        .filter(|f| match f.file_name().to_str() {
            Some(file) => file.ends_with("json") && file.starts_with("dump"),
            None => false,
        })
        .inspect(|f| println!("Reading over {:?}", f.path()))
        .flat_map(|f| std::fs::read_to_string(f.path()))
        .flat_map(|st| serde_json::from_str::<DumpData>(&st))
        .collect();
    let mut file_dump = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open("full_dump.json")
        .unwrap();
    let dump_string = serde_json::to_string(&dumps).unwrap();
    file_dump.write_all(dump_string.as_bytes()).unwrap();
}
