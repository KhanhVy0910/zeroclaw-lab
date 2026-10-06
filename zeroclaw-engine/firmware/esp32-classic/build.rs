fn main() {
    embuild::espidf::sysenv::output();
    println!("cargo:rerun-if-changed=sdkconfig.defaults");
    println!("cargo:rerun-if-changed=partitions.csv");
}
