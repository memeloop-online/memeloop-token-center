use std::{env, fs, process::ExitCode};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() != 2 {
        eprintln!("usage: componentize <core-module.wasm> <output-component.wasm>");
        return ExitCode::FAILURE;
    }
    let module = match fs::read(&args[0]) {
        Ok(module) => module,
        Err(error) => {
            eprintln!("failed to read {}: {error}", args[0]);
            return ExitCode::FAILURE;
        }
    };
    let component = match wit_component::ComponentEncoder::default()
        .module(&module)
        .and_then(|mut encoder| encoder.encode())
    {
        Ok(component) => component,
        Err(error) => {
            eprintln!("failed to componentize {}: {error:?}", args[0]);
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = fs::write(&args[1], &component) {
        eprintln!("failed to write {}: {error}", args[1]);
        return ExitCode::FAILURE;
    }
    println!("wrote {} ({} bytes)", args[1], component.len());
    ExitCode::SUCCESS
}
