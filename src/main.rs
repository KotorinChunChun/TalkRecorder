#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod entry;

fn main() -> anyhow::Result<()> {
    entry::run(false)
}
