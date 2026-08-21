fn main() {
    for variable in [
        "MTC_BUILD_GIT_SHA",
        "MTC_BUILD_TIMESTAMP",
        "MTC_BUILD_TARGET",
    ] {
        println!("cargo:rerun-if-env-changed={variable}");
    }
}
