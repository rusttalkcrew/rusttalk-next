//! The bundled picture model (#682) loads and scores ordinary pictures within a time
//! and memory budget, and blurs none of them. Its own test binary, so the memory it
//! measures is the model's alone. The pictures are self-made (pictures/draw.py).

use std::time::{Duration, Instant};

use rusttalk_client::{SENSITIVE_THRESHOLD, sensitive_model};

const PICTURES: [(&str, &[u8]); 4] = [
    ("landscape", include_bytes!("pictures/landscape.png")),
    ("food", include_bytes!("pictures/food.png")),
    ("fruit", include_bytes!("pictures/fruit.png")),
    ("cartoon", include_bytes!("pictures/cartoon.png")),
];

/// Loading includes parsing and optimizing the model, once per app run.
const LOAD_BUDGET: Duration = Duration::from_secs(5);
/// One picture on a desktop; tests build tract optimized (see Cargo.toml).
const PICTURE_BUDGET: Duration = Duration::from_millis(500);
/// Resident memory the loaded model and a check may add.
const MEMORY_BUDGET_MB: u64 = 120;

/// This process's resident memory in MB, where the system says.
fn resident_mb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    Some(line.split_whitespace().nth(1)?.parse::<u64>().ok()? / 1024)
}

#[test]
fn ordinary_pictures_are_not_blurred_within_budget() {
    let before = resident_mb();
    let mut times = Vec::new();
    for (name, bytes) in PICTURES {
        let picture = image::load_from_memory(bytes).unwrap().into_rgba8();
        let started = Instant::now();
        let classes = sensitive_model::classes(&picture).unwrap();
        times.push(started.elapsed());
        let nude = sensitive_model::nude(&classes);
        println!(
            "{name}: nude {nude:.3}, classes {classes:?}, {:?}",
            times.last().unwrap()
        );
        assert!(
            nude < SENSITIVE_THRESHOLD,
            "{name} would be blurred: {nude}"
        );
        assert!((classes.iter().sum::<f32>() - 1.0).abs() < 0.01);
    }
    // The first check loads the model too.
    assert!(times[0] < LOAD_BUDGET, "loading took {:?}", times[0]);
    for time in &times[1..] {
        assert!(*time < PICTURE_BUDGET, "a picture took {time:?}");
    }
    if let (Some(before), Some(after)) = (before, resident_mb()) {
        println!("memory: {before} MB before, {after} MB after");
        assert!(
            after.saturating_sub(before) < MEMORY_BUDGET_MB,
            "{before} → {after} MB"
        );
    }
}
