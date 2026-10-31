//! Prints the interfaces SoundSync would announce on (`--interface auto`).
fn main() {
    for n in soundsync_engine::discovery::local_networks("auto") {
        println!("{} {}/{}", n.name, n.ip, n.prefix);
    }
}
