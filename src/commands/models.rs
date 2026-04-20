use anyhow::Result;
use owo_colors::OwoColorize;

use crate::client::list_models;
use crate::config::load;

pub fn run(_refresh: bool) -> Result<()> {
    let cfg = load()?;
    let list = list_models(&cfg)?;

    let id_width = list
        .data
        .iter()
        .map(|m| m.id.chars().count())
        .max()
        .unwrap_or(2)
        .max("ID".len());

    println!(
        "{:<id_width$}  {}",
        "ID".bold(),
        "OWNED BY".bold(),
        id_width = id_width
    );
    for m in &list.data {
        let owner = m.owned_by.as_deref().unwrap_or("-");
        println!("{:<id_width$}  {}", m.id, owner, id_width = id_width);
    }
    Ok(())
}
