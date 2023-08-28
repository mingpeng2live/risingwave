// Copyright 2023 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use core::option::Option::Some;
use core::result::Result::{Err, Ok};
use std::fs;
use std::path::Path;
use std::sync::{Arc, LazyLock};

use jni::{InitArgsBuilder, JNIVersion, JavaVM};
use risingwave_pb::connector_service::GetEventStreamResponse;
use tokio::sync::mpsc::Sender;

pub static JVM: LazyLock<Arc<JavaVM>> = LazyLock::new(|| {
    let dir_path = ".risingwave/bin/connector-node/libs/";

    let dir = Path::new(dir_path);

    if !dir.is_dir() {
        panic!("{} is not a directory", dir_path);
    }

    let mut class_vec = vec![];

    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.path().file_name() {
                println!("{:?}", name);
                class_vec.push(dir_path.to_owned() + name.to_str().to_owned().unwrap());
            }
        }
    } else {
        println!("failed to read directory {}", dir_path);
    }

    // Build the VM properties
    let jvm_args = InitArgsBuilder::new()
        // Pass the JNI API version (default is 8)
        .version(JNIVersion::V8)
        // You can additionally pass any JVM options (standard, like a system property,
        // or VM-specific).
        // Here we enable some extra JNI checks useful during development
        // .option("-Xcheck:jni")
        .option("-ea")
        .option(format!("-Djava.class.path={}", class_vec.join(":")))
        .option("-agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=9111")
        .build()
        .unwrap();

    // Create a new VM
    let jvm = match JavaVM::new(jvm_args) {
        Err(err) => {
            panic!("{:?}", err)
        }
        Ok(jvm) => jvm,
    };

    Arc::new(jvm)
});

pub type MyJniSender = Sender<GetEventStreamResponse>;
