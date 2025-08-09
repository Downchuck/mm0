## Working with this repository

This repository contains the Metamath Zero proof assistant and related tools. The primary implementation is `mm0-rs`, written in Rust.

### Building and using `mm0-rs`

To build the `mm0-rs` compiler and language server, you will need to have Rust installed. You can install it from [rustup.rs](https://rustup.rs/).

Once Rust is installed, follow these steps:

1.  Navigate to the `mm0-rs` directory:
    ```bash
    cd mm0-rs
    ```
2.  Build the project in release mode:
    ```bash
    cargo build --release
    ```
    The executable will be located at `target/release/mm0-rs`.

### Compiling MM1 files

To compile an MM1 file (e.g., from the `examples` directory), use the `compile` command of `mm0-rs`.

```bash
# from the mm0-rs directory
./target/release/mm0-rs compile ../examples/some_file.mm1

# from the root directory
mm0-rs/target/release/mm0-rs compile examples/some_file.mm1
```

You can specify the output file using the `-o` flag. For example:

```bash
# from the mm0-rs directory
./target/release/mm0-rs compile ../examples/hello_mmc.mm1 -o ../examples/hello_mmc.mmb
```

### Verifying MMB files

The `mm0-c` tool can be used to verify `.mmb` files. To build and run `mm0-c`:

1.  Navigate to the `mm0-c` directory:
    ```bash
    cd mm0-c
    ```
2.  Compile the verifier:
    ```bash
    gcc main.c -o mm0-c
    ```
3.  Run the verifier on an `.mmb` file:
    ```bash
    ./mm0-c path/to/file.mmb
    ```

### Compilation Targets: `.mmb` vs `.elf`

There are two primary compilation paths for `.mm1` files, resulting in different output types:

*   **`.mmb` (Proof Verification):** Most `.mm1` files, such as `examples/set.mm1`, are proof scripts that define axioms and theorems. These are compiled into `.mmb` (Metamath Binary) files using the `mm0-rs compile` command. The `.mmb` file contains a binary representation of the proofs, which can then be formally verified by a tool like `mm0-c` against an `.mm0` specification file. This path is used to ensure the logical soundness of a set of proofs.

*   **`.elf` (Executable File):** Some `.mm1` files, like `examples/hello_mmc.mm1`, are designed to produce executable programs. These files import `compiler.mm1` and use special functions (like `mmc-add` and `mmc-finish`) to construct a program. The `output` command in the `.mm1` file is used to generate the final executable code as a string, which is then written to a file. This process results in a verified ELF executable, where the compilation process itself is part of the formal proof. To run such a file, you would compile it with `mm0-rs compile` and then execute the resulting output file.
