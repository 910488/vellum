# Independently named Linux dev-host for one QA sandbox run.
# Does not reuse vellum-dev-host container/alias/port/known-hosts.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("up", "down", "status")]
    [string] $Command,
    [Parameter(Mandatory = $true)]
    [string] $RunId,
    [int] $Port = 0,
    [string] $StateRoot
)

$ErrorActionPreference = "Stop"
$repo = (Resolve-Path (Join-Path $PSScriptRoot "..\..\..")).Path
if (-not $StateRoot) { $StateRoot = Join-Path $repo "qa\runs\$RunId\dev-host" }
New-Item -ItemType Directory -Force -Path $StateRoot | Out-Null

$safe = ($RunId -replace "[^a-zA-Z0-9-]", "").ToLower()
if (-not $safe) { throw "run id produced an empty docker name" }
$container = "vellum-qa-$safe"
$alias = "vellum-qa-$safe"
$image = "vellum-dev-host:latest"
if ($Port -le 0) { $Port = 23000 + (Get-Random -Maximum 1000) }
$keyPath = Join-Path $StateRoot "id_ed25519"
$knownHosts = Join-Path $StateRoot "known_hosts"
$sshConfig = Join-Path $StateRoot "ssh_config"
$connectHost = $null

function Write-State {
    @{
        runId = $RunId
        container = $container
        alias = $alias
        port = $Port
        keyPath = $keyPath
        knownHosts = $knownHosts
        sshConfig = $sshConfig
        connectHost = $connectHost
        firewallRule = "vellum-qa-$safe-ssh"
    } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $StateRoot "host.json") -Encoding utf8
}

if ($Command -eq "down") {
    docker rm -f $container 2>$null | Out-Null
    Get-NetFirewallRule -DisplayName "vellum-qa-$safe-ssh" -ErrorAction SilentlyContinue | Remove-NetFirewallRule -ErrorAction SilentlyContinue
    Write-Output "removed $container"
    exit 0
}

if ($Command -eq "status") {
    docker inspect -f "{{.State.Status}}" $container
    exit $LASTEXITCODE
}

if (-not (Get-Command docker -ErrorAction SilentlyContinue)) {
    throw "Docker is required for the sandbox remote host"
}

if (-not (Test-Path -LiteralPath $keyPath)) {
    ssh-keygen -q -t ed25519 -N '""' -C $alias -f $keyPath | Out-Null
}

docker image inspect $image 2>$null | Out-Null
if ($LASTEXITCODE -ne 0) {
    docker buildx build --load -t $image -f (Join-Path $repo "deploy\dev-host\Dockerfile") (Join-Path $repo "deploy\dev-host")
    if ($LASTEXITCODE -ne 0) { throw "dev-host image build failed" }
}

docker rm -f $container 2>$null | Out-Null
$pub = Get-Content -LiteralPath "$keyPath.pub" -Raw
# Publish on all interfaces so Windows Sandbox can reach the host address; firewall is the run's job.
docker run -d --name $container --privileged -p "${Port}:22" $image | Out-Null
if ($LASTEXITCODE -ne 0) { throw "failed to start $container" }
$pub | docker exec -i $container tee /home/vellum/.ssh/authorized_keys | Out-Null
docker exec $container chown vellum:vellum /home/vellum/.ssh/authorized_keys
docker exec $container chmod 600 /home/vellum/.ssh/authorized_keys

$rule = "vellum-qa-$safe-ssh"
Get-NetFirewallRule -DisplayName $rule -ErrorAction SilentlyContinue | Remove-NetFirewallRule -ErrorAction SilentlyContinue
try {
    New-NetFirewallRule -DisplayName $rule -Direction Inbound -Protocol TCP -LocalPort $Port -Action Allow | Out-Null
} catch {
    Write-Output "firewall rule $rule not created: $($_.Exception.Message)"
}
$connectHost = $null
try {
    $connectHost = (Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue |
        Where-Object { $_.InterfaceAlias -match "vEthernet|Default Switch|WSL" -and $_.IPAddress -notmatch "^127\." } |
        Select-Object -First 1).IPAddress
} catch {}
if (-not $connectHost) { $connectHost = "host.lan" }

$block = @"
Host $alias
    HostName 127.0.0.1
    Port $Port
    User vellum
    IdentityFile $keyPath
    IdentitiesOnly yes
    UserKnownHostsFile $knownHosts
    StrictHostKeyChecking accept-new
    BatchMode yes
"@
Set-Content -LiteralPath $sshConfig -Value $block -Encoding ascii
Write-State
Write-Output (Get-Content -LiteralPath (Join-Path $StateRoot "host.json") -Raw)
