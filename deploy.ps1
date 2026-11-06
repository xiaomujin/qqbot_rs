# 发布：构建 release → 部署到 run\ → 重启（发布环境，独立目录）
#
# 用法：.\deploy.ps1            构建 + 部署 + 重启
#       .\deploy.ps1 -NoRestart  只构建部署，不动正在跑的实例
#
# 为什么要脚本：这几步每一步都能**悄悄**出错，而且出错时看起来都像「没问题」——
#   · 拷错二进制：把 debug 拷进去，大卡片渲染 3.5 秒 → 顶破 5 秒预算 → 降级成文字
#   · 旧实例没退干净：新进程起不来，或者两个实例同时在跑 → 同一条消息回两遍
#   · 配置没带：run\ 缺 config.toml 时 discover 会**向上**找到项目根那份，
#     「独立目录」当场变成假的，而启动照样成功
param([switch]$NoRestart)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$run = Join-Path $root 'run'
$built = Join-Path $root 'target\release\qqbot.exe'

cargo build --release --manifest-path (Join-Path $root 'Cargo.toml')
if ($LASTEXITCODE -ne 0) { throw 'cargo build --release 失败' }

New-Item -ItemType Directory -Force -Path $run, (Join-Path $run 'data'), (Join-Path $run 'logs') | Out-Null
$exe = Join-Path $run 'qqbot.exe'
Copy-Item -Force $built $exe
# 旧实例占着 exe 时 Copy-Item 会报错但不中断 —— 必须校验哈希，别信它有没有抛
if ((Get-FileHash $built).Hash -ne (Get-FileHash $exe).Hash) { throw '二进制没拷成功' }

# 发布目录必须自带配置（首次：copy config.toml run\config.toml）
$cfg = Join-Path $run 'config.toml'
if (-not (Test-Path $cfg)) {
    throw "缺少 $cfg —— 发布目录要自带一份配置，不能靠向上找到项目根那份"
}

if ($NoRestart) { Write-Output "已部署到 $run（未重启）"; return }

# 同一 AppID 只能有一个实例在线：发布与测试同时跑会让同一条消息回两遍
Get-Process qqbot -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 2
if (@(Get-Process qqbot -ErrorAction SilentlyContinue).Count -ne 0) { throw '旧实例没退干净' }

foreach ($f in 'live.log', 'live.err.log') {
    $p = Join-Path $run "logs\$f"
    if (Test-Path $p) { Move-Item -Force $p "$p.prev" }
}

$env:RUST_LOG = 'info'
$start = @{
    FilePath               = $exe
    ArgumentList           = 'run'
    WorkingDirectory       = $run
    RedirectStandardOutput = (Join-Path $run 'logs\live.log')
    RedirectStandardError  = (Join-Path $run 'logs\live.err.log')
    WindowStyle            = 'Hidden'
}
Start-Process @start
Start-Sleep -Seconds 6

Get-Process qqbot | Select-Object Id, Path | Format-Table -AutoSize
Get-Content (Join-Path $run 'logs\live.log') -Tail 4
