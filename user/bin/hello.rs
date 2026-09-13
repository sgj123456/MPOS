//! hello: the first mpos user program, std version.

fn main() {
    println!("hello from ring 3");

    // std 没有直接暴露 yield/sleep 的系统调用，这里用 std 的线程睡眠
    std::thread::sleep(std::time::Duration::from_millis(150));

    let pid = std::process::id();
    println!("hello: pid = {}", pid);

    println!("hello: back from sleep, exiting");
    std::process::exit(42);
}
