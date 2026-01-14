use convert_invert::internals::search::search_manager::DumpData;
use std::{fs::OpenOptions, io::Write};

#[allow(clippy::useless_conversion)]
fn main() {
    let date = chrono::Local::now().to_string();
    let dumps = std::fs::read_dir(".")
        .unwrap()
        .into_iter()
        .flatten()
        .filter(|f| match f.file_name().to_str() {
            Some(file) => {
                file.ends_with("json")
                    && file.starts_with("dump_")
                    && file.chars().any(|x| x.is_uppercase())
            }
            None => false,
        })
        .inspect(|f| println!("Reading over {:?}", f.path()))
        .flat_map(|f| {
            let file = format!("Cannot delete {:?}", f.path());
            std::fs::remove_file(f.path()).expect(&file);
            std::fs::read_to_string(f.path())
        })
        .flat_map(|st| serde_json::from_str::<DumpData>(&st))
        .collect::<Vec<_>>();
    let file_name = format!("full_dump_{}.json", date);
    let mut file_dump = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(file_name)
        .unwrap();
    let dump_string = serde_json::to_string(&dumps).unwrap();
    file_dump.write_all(dump_string.as_bytes()).unwrap();
}
