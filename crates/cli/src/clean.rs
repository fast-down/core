use crate::persist;
use color_eyre::Result;

pub fn clean() -> Result<()> {
    let conn = persist::init_db()?;
    let len = conn.execute(
        "DELETE FROM write_progress WHERE progress = '0-' || (total_size - 1)",
        [],
    )?;
    println!("已清理 {len} 行链接");
    Ok(())
}
