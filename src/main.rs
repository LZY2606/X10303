use lsm_review::{casepack, server, store};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let mut addr: SocketAddr = "127.0.0.1:5390".parse().unwrap();
    let mut db = PathBuf::from("lsm-review.db");
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--addr" if i + 1 < args.len() => {
                addr = args[i + 1].parse().expect("无效地址");
                i += 2;
            }
            "--db" if i + 1 < args.len() => {
                db = PathBuf::from(&args[i + 1]);
                i += 2;
            }
            _ => i += 1,
        }
    }
    let store = store::Store::open(&db).expect("打开数据库失败");
    casepack::load_casepack(&store).expect("加载案例包失败");
    let state = Arc::new(server::AppState { store });
    let app = server::build_router(state);
    println!("LSM 审阅服务启动: http://{}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await.expect("绑定端口失败");
    axum::serve(listener, app).await.expect("服务运行失败");
}
