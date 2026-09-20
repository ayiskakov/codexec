use std::io::{self, Read};

fn main() {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input).unwrap();
    let sum: i64 = input.split_whitespace().map(|t| t.parse::<i64>().unwrap()).sum();
    println!("{sum}");
}
