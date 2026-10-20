//! nodeへ置き、collectorがSSH越しに呼ぶ集計binary。依存はregexだけにし、muslの静的binaryとして
//! 手元からcross buildできるようにする。集計は手元のisuscopeと同じfileを取り込む。

#[path = "../../src/access_summary.rs"]
mod access_summary;

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    std::process::exit(access_summary::cli(&args));
}
