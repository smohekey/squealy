use squealy::*;

fn main() {
    // A searched CASE needs at least one WHEN; `otherwise`/`end` are only available once an arm has
    // been added, so an empty `CASE END` / `CASE ELSE … END` cannot be built.
    let _no_arms = case::<i32>().end();
    let _no_arms_else = case::<i32>().otherwise(0);
}
