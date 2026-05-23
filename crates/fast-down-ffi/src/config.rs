use fast_down::{ProgressEntry, Proxy};
use parking_lot::Mutex;
use std::{collections::HashMap, net::IpAddr, sync::Arc, time::Duration};

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum WriteMethod {
    #[default]
    Mmap,
    Std,
}

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone)]
pub struct Config {
    /// 线程数量，推荐值 `32` / `16` / `8`。线程越多不意味着越快
    pub threads: usize,
    /// 设置代理，支持 https、http、socks5 代理
    pub proxy: Proxy<String>,
    /// 自定义请求头
    pub headers: HashMap<String, String>,
    /// 最小分块大小，单位为字节，推荐值 `8 * 1024 * 1024`
    ///
    /// - 分块太小容易造成强烈竞争
    /// - 当无法分块的时候会进入冗余竞争模式
    pub min_chunk_size: u64,
    /// 是否确保文件已经写入磁盘，推荐值 `false`
    ///
    /// 除非你需要下载完成后立马关机，不然都应该用 `false`
    pub sync_all: bool,
    /// 写入缓冲区大小，单位为字节，推荐值 `16 * 1024 * 1024`
    ///
    /// - 只对 [`WriteMethod::Std`] 写入方法有效，有利于将随机写入转换为顺序写入，提高写入速度
    /// - 对于 [`WriteMethod::Mmap`] 写入方法无效，因为写入缓冲区由系统决定
    pub write_buffer_size: usize,
    /// 字节合并区最大大小，单位为字节，推荐值 `16 * 1024 * 1024` 或比 `write_buffer_size` 略大
    ///
    /// 当字节合并区大小 >= `cache_high_watermark` 会触发一次合并写入，把字节合并区的大小降到 `cache_low_watermark` 及以下
    ///
    /// - 只对 [`WriteMethod::Std`] 写入方法有效，有利于将随机写入转换为顺序写入，提高写入速度
    /// - 对于 [`WriteMethod::Mmap`] 写入方法无效
    pub cache_high_watermark: usize,
    /// 字节合并区目标大小，单位为字节，推荐值 `8 * 1024 * 1024` 或为 `cache_high_watermark` 的一半
    ///
    /// 字节合并区在触发合并写入后，大小会降到 `cache_low_watermark` 及以下
    ///
    /// - 只对 [`WriteMethod::Std`] 写入方法有效，有利于将随机写入转换为顺序写入，提高写入速度
    /// - 对于 [`WriteMethod::Mmap`] 写入方法无效
    pub cache_low_watermark: usize,
    /// 写入队列容量，推荐值 `10240`
    ///
    /// 如果下载线程太快，填满了写入队列，会触发压背，降低下载速度，防止内存占用过大
    pub write_queue_cap: usize,
    /// 请求失败后的默认重试间隔，推荐值 `500ms`
    ///
    /// 如果服务器返回中有 `Retry-After` 头，则遵循服务器返回的设定
    pub retry_gap: Duration,
    /// 拉取超时时间，推荐值 `5000ms`
    ///
    /// 请求发出后，接收字节中，如果在 `pull_timeout` 这一段时间内一个字节也没收到，则中断连接，重新请求。
    /// 有利于触发 TCP 重新检测拥塞状态，提高下载速度
    pub pull_timeout: Duration,
    /// 是否接受无效证书（危险），推荐值 `false`
    pub accept_invalid_certs: bool,
    /// 是否接受无效主机名（危险），推荐值 `false`
    pub accept_invalid_hostnames: bool,
    /// 写入磁盘方式，推荐值 [`WriteMethod::Mmap`]
    ///
    /// - [`WriteMethod::Mmap`] 写入方式速度最快，将写入交给操作系统执行，但是：
    ///     1. 在 32 位系统上最大只能映射 4GB 的文件，所以在 32 位系统上，会自动回退到 [`WriteMethod::Std`]
    ///     2. 必须知道文件大小，否则会自动回退到 [`WriteMethod::Std`]
    ///     3. 特殊情况下会出现系统把所有数据全部缓存在内存中，下载完成后一次性写入磁盘，造成下载完成后长时间卡顿
    /// - [`WriteMethod::Std`] 写入方式兼容性最好，会在 `write_buffer_size` 内对片段进行排序，尽量转换为顺序写入
    #[cfg(feature = "file")]
    pub write_method: WriteMethod,
    /// 设置获取元数据的重试次数，推荐值 `10`。注意，这不是下载中的重试次数
    pub retry_times: usize,
    /// 使用哪些地址来发送请求，推荐值 `Vec::new()`
    ///
    /// 如果你有多个网卡可用，可以填写他们的对外 IP 地址，请求会在这些 IP 地址上轮换，下载不一定会更快
    pub local_address: Vec<IpAddr>,
    /// 冗余线程数，推荐值 `3`
    ///
    /// 当块大小小于 `min_chunk_size` 后无法分块，进入冗余竞争模式。
    /// 最多有 `max_speculative` 个线程在同一分块上竞争下载，以解决下载卡进度 99% 的问题
    pub max_speculative: usize,
    /// 已经下载过的部分，如果你想下载整个文件，就传 `Vec::new()`
    pub downloaded_chunk: Arc<Mutex<Vec<ProgressEntry>>>,
    /// 已下载分块的平滑窗口，单位为字节，推荐值 `8 * 1024`
    ///
    /// 它会过滤掉 `downloaded_chunk` 中小于 `chunk_window` 的小空洞，以减小 HTTP 请求数量
    pub chunk_window: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            retry_times: 10,
            threads: 32,
            proxy: Proxy::System,
            headers: HashMap::new(),
            sync_all: false,
            min_chunk_size: 8 * 1024 * 1024,
            write_buffer_size: 16 * 1024 * 1024,
            cache_high_watermark: 16 * 1024 * 1024,
            cache_low_watermark: 8 * 1024 * 1024,
            write_queue_cap: 10240,
            retry_gap: Duration::from_millis(500),
            pull_timeout: Duration::from_secs(5),
            accept_invalid_certs: false,
            accept_invalid_hostnames: false,
            local_address: Vec::new(),
            max_speculative: 3,
            #[cfg(feature = "file")]
            write_method: WriteMethod::Mmap,
            downloaded_chunk: Arc::default(),
            chunk_window: 8 * 1024,
        }
    }
}
