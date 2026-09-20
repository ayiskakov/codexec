use std::collections::HashMap;
use std::io::{self, Read};

fn main() {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input).unwrap();
    let mut tokens = input.split_ascii_whitespace().map(|t| t.parse::<i64>().unwrap());
    let n = tokens.next().unwrap() as usize;
    let target = tokens.next().unwrap();
    let mut seen: HashMap<i64, usize> = HashMap::with_capacity(n);
    for (j, value) in tokens.take(n).enumerate() {
        if let Some(i) = seen.get(&(target - value)) {
            println!("{i} {j}");
            return;
        }
        seen.insert(value, j);
    }
}
