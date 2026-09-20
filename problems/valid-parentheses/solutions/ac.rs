use std::io::{self, BufRead};

fn valid(s: &str) -> bool {
    let mut stack = Vec::with_capacity(s.len());
    for ch in s.bytes() {
        match ch {
            b'(' | b'[' | b'{' => stack.push(ch),
            b')' => if stack.pop() != Some(b'(') { return false },
            b']' => if stack.pop() != Some(b'[') { return false },
            b'}' => if stack.pop() != Some(b'{') { return false },
            _ => {}
        }
    }
    stack.is_empty()
}

fn main() {
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line).unwrap();
    println!("{}", valid(line.trim()));
}
