use crate::persist::Database;
use color_eyre::Result;

pub fn clean() -> Result<()> {
    let db = Database::new()?;
    let len = db.clean()?;
    println!("已清理 {len} 行链接");
    Ok(())
}
