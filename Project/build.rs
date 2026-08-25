fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let version = env!("CARGO_PKG_VERSION");
    let parts: Vec<u16> = version
        .split('.')
        .filter_map(|s| s.parse::<u16>().ok())
        .collect();
    let major = parts.first().copied().unwrap_or(0);
    let minor = parts.get(1).copied().unwrap_or(0);
    let build = parts.get(2).copied().unwrap_or(0);
    let revision = parts.get(3).copied().unwrap_or(0);
    // FILEVERSION/PRODUCTVERSION 需要打包为 64 位: major.minor.build.revision
    let packed = (u64::from(major) << 48)
        | (u64::from(minor) << 32)
        | (u64::from(build) << 16)
        | u64::from(revision);

    // FileVersion 为 4 段（缺省补 0），ProductVersion 为版本号本身
    let file_version = format!("{}.{}.{}.{}", major, minor, build, revision);

    // 当前年份（本地时区，保持 Copyright 年份动态更新）
    let year = chrono::Local::now().format("%Y");

    let mut res = winresource::WindowsResource::new();
    res.set_version_info(winresource::VersionInfo::FILEVERSION, packed);
    res.set_version_info(winresource::VersionInfo::PRODUCTVERSION, packed);
    res.set("FileVersion", &file_version);
    res.set("ProductVersion", version);
    res.set("CompanyName", "NXRKYMANE SOFTWARE");
    res.set("ProductName", "Osmium");
    res.set("FileDescription", "osmium64");
    res.set(
        "LegalCopyright",
        &format!("Copyright (C) {} NXRKYMANE SOFTWARE", year),
    );
    // 语言元数据固定为英文（en-US），避免随编译环境变化
    res.set_language(0x0409);
    res.set_icon("../Misc/Osmium.ico");
    res.compile().unwrap();
}
