mod sync_channel;
mod fstar;
mod lsp;

use tower_lsp::{LspService, Server};

#[tokio::main]
async fn main() {
    let log_file = std::fs::File::create("/tmp/fstar-lsp.log").unwrap();
    tracing_subscriber::fmt()
        .with_writer(std::sync::Mutex::new(log_file))
        .with_file(true)
        .with_line_number(true)
        .with_max_level(tracing::Level::DEBUG)
        .init();

    let (stdin, stdout) = (tokio::io::stdin(), tokio::io::stdout());
    let (service, socket) = LspService::build(lsp::Backend::new)
        .custom_method("fstar-lsp/verifyAll", lsp::Backend::verify_all)
        .custom_method("fstar-lsp/laxToPosition", lsp::Backend::lax_to_position)
        .custom_method("fstar-lsp/verifyToPosition", lsp::Backend::verify_to_position)
        .custom_method("fstar-lsp/cancelAll", lsp::Backend::cancel_all)
        .custom_method("fstar-lsp/reloadDependencies", lsp::Backend::reload_dependencies)
        .custom_method("fstar-lsp/restartZ3", lsp::Backend::restart_z3)
        .finish()
    ;
    Server::new(stdin, stdout, socket).serve(service).await;
}
