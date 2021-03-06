#![allow(dead_code)]

extern crate clap;

#[cfg(any(target_os="linux", target_os="macos"))]
extern crate libc;

mod error;
mod ast;
mod parser;
mod interp;
mod typeck;
mod borrowck;
mod ir;
mod jit;
mod x86;
mod intrinsics;
// mod llvm;

use atty;
use log::{info, error};
use std::path::{Path, PathBuf};
use std::{env, fs, time};
use clap::{App, Arg, AppSettings};
use termcolor::ColorChoice;
use crate::ast::{File, Item, intern_string};
use crate::parser::{parse_file, parse_run_code};
use crate::intrinsics::get_intrinsic_ast_items;
use crate::interp::{create_interp_context, interp_file, interp_entry_point};
use crate::typeck::{create_type_context, type_check_file};
use crate::borrowck::borrow_check_file;
use crate::ir::{create_ir_builder, build_ir_from_ast};
use crate::x86::{compile_ir_to_x86_machine_code};
use crate::jit::{allocate_jit_code, finalize_jit_code, execute_jit_code};
// use crate::llvm::codegen_test;

struct Config {
    input: Option<String>,
    run: Option<String>,
    backend: Backend,
    print: Print,
    color_choice: ColorChoice,
    profile: bool,
    type_checking: bool,
    borrow_checking: bool,
    compiletest: bool,
}

enum Backend {
    Interpreter,
    X86,
    LLVM,
}

enum Print {
    Ast,
    Ir,
    Assembly,
    MachineCode,
    None,
}

pub fn main() {
    if cfg!(debug_assertions) {
        // NOTE(alexander): used for debugging without arguments
        let config = Config {
            input: Some(String::from("c:/dev/compiler/examples/fib.sq")),
            // input: None,
            // run: Some(String::from("let x: i32 = if true { false } else { 20 };")),
            run: None,
            backend: Backend::X86,
            print: Print::Assembly,
            // print: Print::None,
            color_choice: ColorChoice::Auto,
            profile: false,
            type_checking: true,
            borrow_checking: true,
            compiletest: false,
        };
        run_compiler(&config);
        return;
    }
    
    let matches = App::new("firstc")
        .setting(AppSettings::ArgRequiredElseHelp)
        .arg(Arg::with_name("INPUT")
             .help("The input source file to compile")
             .value_name("FILE")
             .index(1))
        .arg(Arg::with_name("run")
             .short("r")
             .value_name("CODE")
             .help("Runs the code immediately before executing main"))
        .arg(Arg::with_name("backend")
             .long("backend")
             .help(r#"Compiler backend "interp", "x86", "llvm" (default is "interpreter")"#)
             .value_name("BACKEND")
             .takes_value(true)
             .default_value("interp"))
        .arg(Arg::with_name("print")
             .long("print")
             .help(r#"Print info "ast", "ir", "asm", "machinecode", "none" (default is "none")"#)
             .value_name("BACKEND")
             .takes_value(true)
             .default_value("none"))
        .arg(Arg::with_name("profile")
             .long("profile")
             .help("Timer for the entire execution of the program"))
        .arg(Arg::with_name("version")
             .short("V")
             .long("version")
             .help("Print version output and exit"))
        .arg(Arg::with_name("color")
             .long("color")
             .help(r#"Color preference "always", "ansi", "auto", "off" (default is "auto")"#)
             .value_name("PREFERENCE")
             .takes_value(true)
             .default_value("auto"))
        .arg(Arg::with_name("Znotypecheck")
             .long("Znotypecheck")
             .help("Runs the compiler without type checking")
             .hidden(true))
        .arg(Arg::with_name("Znoborrowcheck")
             .long("Znoborrowcheck")
             .help("Runs the compiler without borrow checking")
             .hidden(true))
        .arg(Arg::with_name("Zcompiletest")
             .long("Zcompiletest")
             .help("Runs the compiler in testing mode")
             .hidden(true))
        .get_matches();

    let mut skip_compilation = false;
    if matches.is_present("version") {
        const VERSION: Option<&'static str> = option_env!("CARGO_PKG_VERSION");
        println!("firstc {}", VERSION.unwrap_or("unknown version"));
        skip_compilation = true;
    }

    let color_choice = match matches.value_of("color").unwrap().to_lowercase().as_str() {
        "always" => ColorChoice::Always,
        "ansi" => ColorChoice::AlwaysAnsi,
        "auto" => {
            if atty::is(atty::Stream::Stdout) {
                ColorChoice::Auto
            } else {
                ColorChoice::Never
            }
        }
        "off" => ColorChoice::Never,
        _ => {
            println!("\n--color expectes one of these values \"always\", \"ansi\", \"auto\", \"off\".\n");
            skip_compilation = true;
            ColorChoice::Never
        }
    };

    let backend = match matches.value_of("backend").unwrap().to_lowercase().as_str() {
        "interp" => Backend::Interpreter,
        "x86" => Backend::X86,
        "llvm" => Backend::LLVM,
        _ => {
            println!("\n--backend expectes one of these values \"interp\", \"x86\", \"llvm\"\n");
            skip_compilation = true;
            Backend::Interpreter
        }
    };

    let print = match matches.value_of("print").unwrap().to_lowercase().as_str() {
        "ast" => Print::Ast,
        "ir" => Print::Ir,
        "asm" => Print::Assembly,
        "machinecode" => Print::MachineCode,
        "none" => Print::None,
        _ => {
            println!("\n--print expectes one of these values \"ast\", \"ir\", \"asm\", \"machinecode\", \"none\"\n");
            skip_compilation = true;
            Print::None
        }
    };
    
    if !skip_compilation {
        let config = Config {
            input: matches.value_of("INPUT").map(|s| s.to_string()),
            run: matches.value_of("run").map(|s| s.to_string()),
            profile: matches.is_present("profile"),
            type_checking: !matches.is_present("Znotypecheck"),
            borrow_checking: !matches.is_present("Znoborrowcheck"),
            compiletest: matches.is_present("Zcompiletest"),
            backend,
            print,
            color_choice,
        };

        run_compiler(&config);
    }
}

fn run_compiler(config: &Config) {
    info!("setting up the compiler");

    error::COLOR_CHOICE.with(|color_choice| {
        *color_choice.borrow_mut() = config.color_choice;
    });
    
    let mut _working_dir = env::current_dir().unwrap_or(PathBuf::new());

    // Parse optional code directly from the config
    let has_run_code = if let Some(source) = &config.run {
        let filename = "<run>";
        let mut ast = parse_run_code(source.to_string(), String::from(filename));

        // Include compiler intrinsics in the parsed ast file    
        let intrinsic_mod = get_intrinsic_ast_items();
        ast.items.push(intrinsic_mod);

        run_parsed_code(ast, &config);
        true
    } else {
        false
    };

    // Parse input file provided by config
    let mut ast = if let Some(input) = &config.input {
        let path = Path::new(input);
        let source;
        match fs::read_to_string(&input) {
            Ok(string) => source = string,
            Err(err) => {
                eprintln!("error: {}", err);
                return;
            }
        };
        
        let mut filename = String::clone(&input);
        if path.is_absolute() {
            _working_dir = path.parent().map(|p| p.to_path_buf()).unwrap_or(_working_dir);
            filename = String::from(path.file_name().map(|s| s.to_str().unwrap()).unwrap());
        }

        // Parse input file
        parse_file(source, filename)
    } else {
        if !has_run_code {
            eprintln!("\nerror: no input file or code");
        }
        return;
    };

    // Include compiler intrinsics in the parsed ast file    
    let intrinsic_mod = get_intrinsic_ast_items();
    ast.items.push(intrinsic_mod);

    if ast.error_count > 0 {
        error!("parse errors reported {} errors, stopping compilation", ast.error_count);
        eprintln!("\nerror: aborting due to previous error");
        return;
    }

    if let Print::Ast = config.print {
        print!("\n\n{:#?}", ast.items);
    }

    run_parsed_code(ast, config);
}


fn run_parsed_code(ast: File, config: &Config) {

    // Type check the current file
    if config.type_checking {
        let mut tc = create_type_context();
        type_check_file(&mut tc, &ast);
        if tc.error_count > 0 {
            error!("type checker reported {} errors, stopping compilation", tc.error_count);
            eprintln!("\nerror: aborting due to previous error");
            return;
        }
    }

    // Borrow check the current file
    if config.borrow_checking {
        let borrow_error_count = borrow_check_file(&ast);
        if borrow_error_count > 0 {
            error!("borrow checker reported {} errors, stopping compilation", borrow_error_count);
            eprintln!("\nerror: aborting due to previous error");
            return;
        }
    }

    // Check if there is a main function
    let main_symbol = intern_string("main");
    let mut has_main = false;
    for item in &ast.items {
        if let Item::Fn(func) = item {
            if func.ident.sym == main_symbol {
                has_main = true;
                break;
            }
        }
    }

    if !has_main {
        eprintln!("\nerror: no main function was found");
        return;
    }

    match config.backend {
        Backend::Interpreter => {
            // Interpret the current file
            let now = time::Instant::now();
            let mut ic = create_interp_context();
            interp_file(&mut ic, &ast);
            let code = interp_entry_point(&mut ic);
            let execution_time = now.elapsed().as_secs_f32();
            println!("\nInterpreter exited with code {}", code);
            if config.profile {
                println!("Interpreter execution time: {} seconds", execution_time)
            }
            return;
        }

        Backend::X86 => {
            // Build low-level intermediate representation
            let mut ir_builder = create_ir_builder();

            // build lir
            build_ir_from_ast(&mut ir_builder, &ast);
            if let Print::Ir = config.print {
                print!("\n\n{}", ir_builder);
            }

            // The resulting intermediate representation
            let ir_instructions = ir_builder.instructions;
            let ir_functions = ir_builder.functions;

            // Generate code to jit
            let (machine_code, assembly) = compile_ir_to_x86_machine_code(ir_instructions, ir_functions);

            if let Print::Assembly = config.print {
                println!("\n\n{}", assembly);
            }

            if let Print::MachineCode = config.print {
                println!("\n");
                for (i, byte) in machine_code.iter().enumerate() {
                    print!("{:02x} ", byte);
                    if i % 16 == 15 {
                        println!("");
                    }
                }
                println!("\n\nSize of code is {} bytes", machine_code.len());
            }

            let jit_code = allocate_jit_code(machine_code.len());
            
            unsafe {
                let src_len = machine_code.len();
                let src_ptr = machine_code.as_ptr();
                std::ptr::copy_nonoverlapping(src_ptr, jit_code.addr, src_len);
            }
            
            finalize_jit_code(&jit_code);
            
            let now = time::Instant::now();
            let ret = execute_jit_code(&jit_code);
            let execution_time = now.elapsed().as_secs_f32();
            println!("\nProgram exited with code {}", ret);
            if config.profile {
                println!("Program execution time: {} seconds", execution_time)
            }
        }

        Backend::LLVM => {
            unimplemented!()
        }
    }
}
