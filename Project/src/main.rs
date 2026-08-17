// ==================== 入口：模块装配 ====================
// 只声明模块并转发给 CLI 入口（service_cli），业务逻辑在 service_*.rs

mod service_cli;
mod service_config;
mod service_host;
mod service_core;
#[cfg(test)]
mod service_tests;

fn main() {
    service_cli::main_entry();
}
