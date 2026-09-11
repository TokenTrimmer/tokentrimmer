//! C05 money-contract DDL guard (cloud/docs/money-contract.md §5.4): money
//! columns in NEW migrations are NUMERIC(12,6) (M1's precision floor), never
//! DOUBLE PRECISION. Migration 0049's reservation columns are the one
//! grandfathered deviation (D2, scheduled for the typed-money migration);
//! every migration after 0049 with a DOUBLE-PRECISION money column is a
//! contract violation caught here.

use std::fs;
use std::path::PathBuf;

#[test]
fn no_double_precision_money_columns_after_migration_0049() {
    let migrations_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut violations: Vec<String> = Vec::new();

    let mut entries: Vec<_> = fs::read_dir(&migrations_dir)
        .expect("read the gateway migrations directory")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|ext| ext == "sql")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".up.sql"))
        })
        .collect();
    entries.sort();

    for path in entries {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        // The migration ordinal (0001, 0049, 0052, …).
        let Ok(ordinal) = name.split('_').next().unwrap_or_default().parse::<u32>() else {
            continue;
        };
        // The one sanctioned deviation (D2): the durable monthly
        // reservation columns predate the contract.
        if ordinal <= 49 {
            continue;
        }
        let sql = fs::read_to_string(&path).unwrap_or_default();
        // A DOUBLE PRECISION column whose identifier names money
        // (_usd / reserved_usd / spend_usd shapes).
        for line in sql.lines() {
            let trimmed = line.trim_start_matches(['-', ' ']).to_ascii_uppercase();
            if trimmed.contains("DOUBLE PRECISION") {
                let lower = line.to_ascii_lowercase();
                let is_money = lower.contains("_usd")
                    || (lower.contains("reserved") && lower.contains("double precision"))
                    || (lower.contains("spend") && lower.contains("double precision"));
                if is_money {
                    violations.push(format!("{name}: {}", line.trim()));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "money columns after migration 0049 must be NUMERIC(12,6) per the \
         C05 money contract (M1), never DOUBLE PRECISION (see D2 for the \
         grandfathered case):\n{}",
        violations.join("\n")
    );
}
