fn main() {
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
    std::env::set_var("PROTOC", protoc);
    tonic_build::compile_protos("proto/runtime.proto").expect("compile runtime.proto");
}
