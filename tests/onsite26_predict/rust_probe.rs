use std::fs::{self, File};
use std::io::Write;
use std::sync::{Arc, Barrier};
use std::thread;

fn main() {
    let root = std::env::args().nth(1).expect("work directory");
    let barrier = Arc::new(Barrier::new(9));
    let mut workers = Vec::new();
    for id in 0..8 {
        let barrier = Arc::clone(&barrier);
        let root = root.clone();
        workers.push(thread::spawn(move || {
            barrier.wait();
            let temporary = format!("{root}/rust-{id}.tmp");
            let final_name = format!("{root}/rust-{id}.txt");
            let mut file = File::create(&temporary).unwrap();
            writeln!(file, "worker={id}").unwrap();
            file.sync_all().unwrap();
            fs::rename(temporary, final_name).unwrap();
        }));
    }
    barrier.wait();
    for worker in workers {
        worker.join().unwrap();
    }
    let count = fs::read_dir(&root)
        .unwrap()
        .filter(|entry| entry.as_ref().unwrap().file_name().to_string_lossy().starts_with("rust-"))
        .count();
    assert_eq!(count, 8);
    println!("STARRY26_RUST_OK");
}
