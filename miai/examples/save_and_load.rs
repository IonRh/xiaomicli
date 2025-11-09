//! 持久化小爱登录状态。
//!
//! 通过 [`Xiaoai::save`] 和 [`Xiaoai::load`]，
//! 可以实现登录状态的保存与加载，而无需每次都重新登录。

use std::{env, fs::File, io::BufReader};

use miai::Xiaoai;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let _ = dotenvy::dotenv();

    const FILENAME: &str = "xiaoai-auth.json";
    let xiaoai = if let Ok(file) = File::open(FILENAME) {
        println!("认证文件 {FILENAME} 存在，尝试加载。");
        let xiaoai = Xiaoai::load(BufReader::new(file)).expect("load");
        println!("成功从 {FILENAME} 加载登录状态！");

        xiaoai
    } else {
        println!("认证文件 {FILENAME} 不存在，尝试重新登录。");
        let username = env::var("MI_USER").expect("env::var");
        let password = env::var("MI_PASS").expect("env::var");
        let xiaoai = Xiaoai::login(&username, &password).await.expect("login");
        println!("登录成功！尝试保存。");

        let mut file = File::create(FILENAME).expect("create");
        xiaoai.save(&mut file).expect("save");
        println!("成功保存为认证文件 {FILENAME}。");

        xiaoai
    };

    let device_info = xiaoai.device_info().await.expect("device_info");
    for info in device_info {
        println!("发现小爱设备 {}。", info.name);
    }
}
