use std::net::SocketAddr;

#[tokio::main]
async fn main() {
    let addr = SocketAddr::from(([127, 0, 0, 1], 3001));
    lane::demo::serve(lane::demo::project_root(), addr)
        .await
        .expect("serve lane server");
}
