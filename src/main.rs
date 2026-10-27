use std::env;

mod storage;

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("Not enough arguments");
        return;
    }

    let cmd = args.get(2).unwrap_or(&String::from("scan"));

    println!("Hello, world!");
}
