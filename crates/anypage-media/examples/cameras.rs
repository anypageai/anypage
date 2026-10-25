//! List the cameras `media.broadcast { source = "camera" }` can use (does not
//! open them): `cargo run --example cameras --features camera`.

fn main() {
    match anypage_media::camera::list() {
        Ok(names) if names.is_empty() => println!("no cameras found"),
        Ok(names) => {
            for (i, n) in names.iter().enumerate() {
                println!("camera {i}: {n}");
            }
        }
        Err(e) => println!("{e}"),
    }
}
