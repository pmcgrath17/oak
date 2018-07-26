/*
 *
 * Copyright 2018 Asylo authors
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 *
 */

#include "absl/strings/str_split.h"
#include "asylo/client.h"
#include "asylo/util/logging.h"
#include "gflags/gflags.h"
#include "oak/oak.pb.h"
#include <iostream>
#include <string>
#include <vector>

DEFINE_string(enclave_path, "", "Path to enclave to load");
DEFINE_string(expressions, "",
              "A comma-separated list of expressions to pass to the enclave");

int main(int argc, char *argv[]) {

  // Setup.
  ::google::ParseCommandLineFlags(&argc, &argv,
                                  /*remove_flags=*/true);

  // Validate flags.
  if (FLAGS_expressions.empty()) {
    LOG(QFATAL)
        << "Must supply a non-empty list of expressions with --expressions";
  }

  std::vector<std::string> expressions = absl::StrSplit(FLAGS_expressions, ',');

  // Initialise enclave.
  asylo::EnclaveManager::Configure(asylo::EnclaveManagerOptions());
  auto manager_result = asylo::EnclaveManager::Instance();
  if (!manager_result.ok()) {
    LOG(QFATAL) << "EnclaveManager unavailable: " << manager_result.status();
  }
  asylo::EnclaveManager *manager = manager_result.ValueOrDie();
  std::cout << "Loading " << FLAGS_enclave_path << std::endl;
  asylo::SimLoader loader(FLAGS_enclave_path, /*debug=*/true);
  asylo::Status status = manager->LoadEnclave("oak_enclave", loader);
  if (!status.ok()) {
    LOG(QFATAL) << "Load " << FLAGS_enclave_path << " failed: " << status;
  }
  LOG(INFO) << "Enclave initialised";

  asylo::EnclaveClient *client = manager->GetClient("oak_enclave");

  // Program enclave with initial script.
  {
    LOG(INFO) << "Programming enclave";
    asylo::EnclaveInput input;
    input.MutableExtension(oak::initialise_input)
        ->set_lisp_script("(define fib (lambda (n) (if (<= n 2) 1 (+ (fib (- n "
                          "1)) (fib (- n 2))))))");
    asylo::EnclaveOutput output;
    status = client->EnterAndRun(input, &output);
    if (!status.ok()) {
      LOG(QFATAL) << "EnterAndRun failed: " << status;
    }
    LOG(INFO) << "Enclave programmed";
  }

  // Evaluate expressions in the enclave.
  for (const auto &expression : expressions) {
    LOG(INFO) << "sending expression to enclave: " << expression;
    asylo::EnclaveInput input;
    input.MutableExtension(oak::evaluate_input)->set_input_data(expression);
    asylo::EnclaveOutput output;
    status = client->EnterAndRun(input, &output);
    if (!status.ok()) {
      LOG(QFATAL) << "EnterAndRun failed: " << status;
    }

    std::cout << "Message from enclave: "
              << output.GetExtension(oak::evaluate_output).output_data()
              << std::endl;
  }

  // Finalization

  LOG(INFO) << "Destroying enclave";
  asylo::EnclaveFinal final_input;
  status = manager->DestroyEnclave(client, final_input);
  if (!status.ok()) {
    LOG(QFATAL) << "Destroy " << FLAGS_enclave_path << " failed: " << status;
  }
  LOG(INFO) << "Enclave destroyed";

  return 0;
}
