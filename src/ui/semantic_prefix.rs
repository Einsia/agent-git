//! Read-only content comparison with its projection boundary kept visible.

use crate::domain::comparison::SemanticPrefix;

pub fn print(prefix: &SemanticPrefix, left: &str, right: &str) {
    match prefix {
        SemanticPrefix::Unavailable(reason) => println!("semantic prefix unavailable ({reason})"),
        SemanticPrefix::Available {
            common,
            left_turns,
            right_turns,
            hash,
        } => {
            println!("semantic prefix  {common} normalized LOG turns");
            if let Some(hash) = hash {
                println!("semantic hash    {}", crate::domain::turn::short(hash));
            }
            println!(
                "{left} semantic suffix  +{} turns    {right} semantic suffix  +{} turns",
                left_turns - common,
                right_turns - common
            );
        }
    }
    println!(
        "semantic comparison uses projected IR, not complete transcript equality or Git ancestry"
    );
    println!(
        "excludes metadata, tool results, compaction and unmodeled content; tool details depend on the adapter"
    );
}
