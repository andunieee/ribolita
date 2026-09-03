example := "counter"
wasm_target := "wasm32-unknown-unknown"
wasm_profile := "release"
wasm_path := "target/" + wasm_target + "/" + wasm_profile + "/example_" + example + ".wasm"
data_dir := "./tmp/ribolita-test"

build:
  cargo build --release -p example_{{example}} --target {{wasm_target}}

build-cli:
  cargo build --release -p cli

# Build GUI app with embedded wasm program
build-slint example="counter":
  #!/usr/bin/env fish
  set -l wasm_path (realpath "target/{{wasm_target}}/{{wasm_profile}}/example_{{example}}.wasm")
  set -l meta_path (realpath "examples/{{example}}/metadata.json")
  PROGRAM=$wasm_path PROGRAM_METADATA=$meta_path cargo build -p slint_app

ndk_version := "27.2.12479018"

# Build Android APK (release)
build-apk example="counter": build
  #!/usr/bin/env fish
  set -l wasm_path (realpath "target/{{wasm_target}}/{{wasm_profile}}/example_{{example}}.wasm")
  set -l meta_path (realpath "examples/{{example}}/metadata.json")
  set -x ANDROID_NDK_HOME (realpath "$HOME/Android/Sdk/ndk/{{ndk_version}}")
  PROGRAM=$wasm_path PROGRAM_METADATA=$meta_path cargo apk build -p slint_app --lib --target aarch64-linux-android --release; or true
  echo "APK: target/release/apk/ribolita.apk"

# Build Android APK (debug)
build-apk-debug example="counter": build
  #!/usr/bin/env fish
  set -l wasm_path (realpath "target/{{wasm_target}}/{{wasm_profile}}/example_{{example}}.wasm")
  set -l meta_path (realpath "examples/{{example}}/metadata.json")
  set -x ANDROID_NDK_HOME (realpath "$HOME/Android/Sdk/ndk/{{ndk_version}}")
  PROGRAM=$wasm_path PROGRAM_METADATA=$meta_path cargo apk build -p slint_app --lib --target aarch64-linux-android; or true
  echo "APK: target/debug/apk/ribolita.apk"

run kind="increase" data="amount=1":
  mkdir -p {{data_dir}}
  cargo run -p cli -- --dir {{data_dir}} --filename {{wasm_path}} entry --kind {{kind}} --data {{data}}
