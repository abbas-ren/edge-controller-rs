You are working on an existing Rust application. Your task is to systematically improve, refactor, test, document, and extend the codebase while preserving existing functionality and backward compatibility wherever reasonably possible.

IMPORTANT PRINCIPLES:

First inspect and understand the existing codebase before making changes.

Do not blindly rewrite working code.

Preserve existing behavior unless a change is explicitly required by this task.

Prefer small, logically grouped changes that can be independently validated.

Do not introduce unnecessary abstractions or dependencies.

Use current stable Rust practices and the latest stable versions of the relevant crates where practical.

Before changing an API, CLI behavior, configuration format, or externally observable behavior, inspect how it is currently used.

Do not remove existing functionality merely to simplify the implementation.

Keep error handling explicit and meaningful.

Avoid duplicated logic.

Keep public APIs well documented.

Follow idiomatic Rust conventions.

After every major stage, format, compile, lint, and run the relevant tests.

Fix regressions immediately before continuing to the next stage.

Do not assume any particular existing project structure. Inspect the repository and determine the appropriate organization from the code itself.

STAGE 1 — CODEBASE AUDIT

Before modifying code:

Inspect the entire Rust codebase and understand:

application entry points

modules and their responsibilities

existing tests

CLI implementation

HTTP/web-server implementation

APIs

configuration

logging

networking

streaming

error handling

external dependencies

existing metrics/observability functionality

Identify:

modules without tests

modules with incomplete test coverage

duplicated functionality

overly large modules

tightly coupled components

magic numbers and string literals

hard-coded configuration values

unclear function/variable names

opportunities to extract submodules

CLI functionality that could be exposed as explicit commands/options

existing HTTP endpoints that could be documented

places where metrics can be collected

places where structured logging would be useful

Do not make broad changes during this audit.

Produce a concise implementation plan based on the actual codebase, then execute the work in the stages below.

STAGE 2 — TEST COVERAGE

Improve the test suite systematically.

For every module:

determine whether tests already exist

identify missing scenarios

add tests where necessary

expand existing tests where coverage is incomplete

Put tests in appropriately named files/modules based on the functionality they test.

Cover, where applicable:

normal/expected behavior

boundary conditions

invalid input

error handling

empty input

large input

configuration variations

networking failures

streaming behavior

concurrency-related behavior

CLI argument handling

API responses

serialization/deserialization

state transitions

Prefer deterministic tests.

Avoid tests that depend unnecessarily on external services.

Add integration tests where unit tests are insufficient.

Do not merely increase the number of tests; make sure the tests exercise meaningful behavior.

Run the complete test suite and fix failures before proceeding.

STAGE 3 — CONSTANTS AND CONFIGURATION

Replace appropriate magic values and repeated constants with named constants.

Identify:

magic numbers

repeated strings

default values

protocol-related constants

timeout values

buffer sizes

port numbers

limits

endpoint paths

metric names

logging-related defaults

Define constants in appropriate constant.rs files associated with the relevant functionality/module.

Use meaningful, descriptive names.

Do not turn values into constants merely for the sake of doing so. Keep genuinely local values local when that improves readability.

Where a value is actually runtime configuration rather than a compile-time constant, model it as configuration instead of a constant.

STAGE 4 — MODULE AND FILE ORGANIZATION

Refactor the source organization based on functionality.

Group related files into appropriate directories/modules.

Identify large modules that contain multiple unrelated responsibilities.

Extract cohesive functionality into submodules where doing so improves:

readability

maintainability

testability

separation of concerns

Avoid excessive fragmentation.

Preserve module visibility and public APIs unless there is a strong reason to change them.

Update imports and module declarations correctly.

Ensure the resulting structure follows idiomatic Rust conventions.

STAGE 5 — NAMING AND API QUALITY

Review function, method, type, constant, module, and variable names throughout the affected code.

Rename unclear or ambiguous names to meaningful, technically accurate names.

Names should communicate:

what the value represents

what the function does

what side effects occur

what abstraction is being represented

Avoid unnecessary abbreviations.

Follow idiomatic Rust naming conventions.

Update all references, tests, documentation, and error messages affected by renaming.

Do not rename public APIs unnecessarily if doing so would break consumers. Where appropriate, preserve compatibility.

STAGE 6 — CLI IMPROVEMENTS

Improve the command-line interface using the latest stable compatible version of clap.

Inspect the current CLI implementation before changing it.

Introduce or improve CLI commands/options/subcommands so that major application functionality can be controlled explicitly.

CLI options should be:

logically grouped

discoverable

consistently named

properly documented

validated

compatible with existing behavior where possible

Add appropriate:

help text

defaults

value validation

enums/subcommands where appropriate

mutually exclusive options where necessary

environment-variable support where useful

Do not add CLI options merely because they are possible. Add options that expose meaningful application functionality.

Add tests for CLI parsing and validation.

Ensure --help output is clear and useful.

STAGE 7 — LOGGING

Implement optional logging capabilities for relevant application activity.

Logging should support, where appropriate:

console logging

file logging

network-related operations

streaming operations

important application lifecycle events

errors

warnings

configuration changes

API requests

significant internal operations

Requirements:

Logging must be controllable through CLI arguments/configuration.

Avoid excessive logging in hot paths.

Never log secrets, credentials, authentication tokens, or sensitive payloads.

Prefer structured logging where appropriate.

Use appropriate log levels.

Ensure logging can be disabled or reduced when required.

Add tests for logging configuration where practical.

STAGE 8 — METRICS AND PROMETHEUS

Add a dedicated metrics subsystem using the latest stable compatible Prometheus Rust crate.

Create a coherent metrics module and appropriate submodules.

Identify useful metrics throughout the application.

Integrate metrics wherever meaningful information can be collected, including where applicable:

request counts

request duration

errors

successful operations

failed operations

bytes processed

bytes transmitted

bytes received

streaming events

active connections

queue sizes

retries

timeouts

cache activity

resource utilization

application-specific operations

Use appropriate Prometheus metric types:

Counter

Gauge

Histogram

Summary only when justified

Use meaningful metric names and labels.

Avoid high-cardinality labels.

Do not introduce metrics that could create unbounded memory usage.

Expose the Prometheus metrics endpoint through the existing web server running on port 8888.

Preserve the existing web server functionality.

Ensure the endpoint returns valid Prometheus exposition data and is usable by Prometheus/Grafana.

Add tests validating metric registration and endpoint behavior.

Document the available metrics and their meaning.

STAGE 9 — ADDITIONAL APIs

Inspect the existing application functionality and identify useful information that can safely be exposed through APIs.

Add APIs where they provide meaningful operational or informational value.

Potential categories include, where applicable:

application status

health

readiness

configuration/status information

runtime statistics

connection information

streaming status

operational counters

resource information

application capabilities

existing internal information that would be useful to external consumers

Requirements:

Follow consistent REST/API conventions.

Use appropriate HTTP methods and status codes.

Validate inputs.

Return useful structured responses.

Provide meaningful error responses.

Add tests for every new API.

Do not expose secrets or sensitive internal information.

Reuse existing web-server infrastructure rather than introducing an unnecessary second server.

STAGE 10 — API DOCUMENTATION / SWAGGER

Add an API documentation module using an appropriate current Rust/OpenAPI-compatible solution.

Generate Swagger/OpenAPI documentation for all externally exposed APIs.

Include:

endpoint paths

HTTP methods

parameters

request bodies

response schemas

status codes

error responses

authentication information if applicable

useful descriptions

examples where useful

Include both existing APIs and newly added APIs.

Expose the Swagger/OpenAPI documentation through the existing web server.

Make the documentation easy to discover.

Keep the API documentation synchronized with the actual API implementation.

Add tests or validation where practical to detect documentation/API drift.

STAGE 11 — DOCUMENTATION

For every new or substantially modified code path:

Add appropriate Rust documentation comments.

Document:

public modules

public types

public functions

important configuration structures

CLI options

APIs

metrics

non-obvious algorithms

important design decisions

Do not add meaningless comments that simply restate the code.

Explain why something is done when the reasoning is not obvious from the implementation.

Keep documentation accurate as the code evolves.

STAGE 12 — QUALITY AND VALIDATION

After completing the implementation:

Run formatting.

Run the complete test suite.

Run Clippy with appropriate warnings enabled.

Build the project in relevant configurations.

Check for:

compiler warnings

Clippy warnings

dead code

unused dependencies

broken documentation

broken imports

API inconsistencies

incorrect CLI behavior

metric registration issues

logging issues

regressions

Fix issues rather than simply reporting them.

Verify that:

existing functionality still works

new CLI functionality works

logging works

metrics are exposed on the existing port 8888 server

Prometheus can consume the metrics endpoint

Swagger/OpenAPI documentation is accessible

new APIs work

tests pass

IMPORTANT IMPLEMENTATION RULES

Work incrementally.

Do not make the entire change as one uncontrolled rewrite.

After each major stage, validate the code before continuing.

Preserve existing functionality.

Prefer backward-compatible changes.

Do not introduce unnecessary dependencies.

Use the latest stable compatible crate versions.

Check crate APIs rather than assuming APIs from memory.

Follow Rust idioms and existing project conventions where they are sound.

Do not duplicate functionality that already exists.

Do not silently remove existing endpoints, CLI options, configuration, or behavior.

Do not hard-code secrets or credentials.

Do not log secrets or sensitive data.

Keep performance in mind, particularly for networking, streaming, and high-frequency code.

Keep metrics low-overhead and avoid high-cardinality labels.

Ensure asynchronous code remains properly asynchronous where the existing application uses async Rust.

Avoid blocking operations inside async execution paths.

Maintain thread/concurrency safety.

FINAL REVIEW

At the end, provide a concise summary containing:

Major architectural changes.

Modules/submodules added or extracted.

Tests added or significantly expanded.

CLI improvements.

Logging capabilities.

Metrics added.

New APIs.

Swagger/OpenAPI changes.

Important dependency changes.

Validation performed:

cargo fmt

cargo test

cargo clippy

cargo build

any additional relevant checks

Also explicitly identify any items that could not be completed and explain why.

Do not claim that a validation step passed unless you actually ran it and observed the result.