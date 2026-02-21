use swift_rs::SwiftLinker;

fn main() {
    SwiftLinker::new("13.0")
        .with_package("PhotoFerrySwift", "./swift/")
        .link();

    // Link required Apple frameworks
    println!("cargo:rustc-link-lib=framework=Photos");
    println!("cargo:rustc-link-lib=framework=CoreLocation");
}
