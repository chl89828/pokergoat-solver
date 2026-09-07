//! §6.4 애그리게이트 리포트. PR4에서 구현한다.

use anyhow::Result;
use std::path::Path;

pub fn run(scenario_dir: &Path, out_dir: &Path) -> Result<()> {
    println!(
        "not implemented (scenario-dir = {}, out = {})",
        scenario_dir.display(),
        out_dir.display()
    );
    Ok(())
}
