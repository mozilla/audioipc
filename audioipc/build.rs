extern crate cc;

fn main() {
    cc::Build::new()
        .file("src/cmsghdr.c")
        .compile("cmsghdr");
}
