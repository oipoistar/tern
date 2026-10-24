# Starts Tern's self-hosted GitHub Actions runner, hidden, in a clean
# environment: this machine's and this user's variables as a fresh sign-in
# has them, never those of whatever started it (an editor, an agent), plus
# the runner's own: Git's bash ahead of WSL's for the workflows' bash steps,
# Rust kept apart from the user's own toolchain, and build output outside
# the checkout, which every run cleans, so builds stay incremental.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File C:\actions-runner\start.ps1

$root = $PSScriptRoot
if (Get-Process -Name Runner.Listener -ErrorAction SilentlyContinue) {
    Write-Output "The runner is already running."
    exit 0
}

# What Windows sets for each sign-in, beside the variables kept in the
# registry.
$session = @(
    'ALLUSERSPROFILE', 'APPDATA', 'CommonProgramFiles', 'CommonProgramFiles(x86)',
    'CommonProgramW6432', 'COMPUTERNAME', 'ComSpec', 'HOMEDRIVE', 'HOMEPATH',
    'LOCALAPPDATA', 'LOGONSERVER', 'NUMBER_OF_PROCESSORS', 'ProgramData',
    'ProgramFiles', 'ProgramFiles(x86)', 'ProgramW6432', 'PUBLIC', 'SESSIONNAME',
    'SystemDrive', 'SystemRoot', 'USERDOMAIN', 'USERDOMAIN_ROAMINGPROFILE',
    'USERNAME', 'USERPROFILE'
)
$machine = [Environment]::GetEnvironmentVariables('Machine')
$user = [Environment]::GetEnvironmentVariables('User')

$start = New-Object System.Diagnostics.ProcessStartInfo
$start.FileName = $env:ComSpec
$start.Arguments = "/d /c `"$root\run.cmd`""
$start.WorkingDirectory = $root
$start.UseShellExecute = $false
$start.CreateNoWindow = $true
$vars = $start.EnvironmentVariables
$vars.Clear()
foreach ($name in $session) {
    $value = [Environment]::GetEnvironmentVariable($name)
    if ($value) { $vars[$name] = $value }
}
foreach ($scope in $machine, $user) {
    foreach ($name in $scope.Keys) {
        if ($name -ne 'Path') {
            $vars[$name] = [Environment]::ExpandEnvironmentVariables($scope[$name])
        }
    }
}
$path = [Environment]::ExpandEnvironmentVariables("$($machine['Path']);$($user['Path'])")
$vars['Path'] = "C:\Program Files\Git\bin;$path"
$vars['CARGO_HOME'] = "$root\cargo"
$vars['RUSTUP_HOME'] = "$root\rustup"
$vars['CARGO_TARGET_DIR'] = ($root -replace '\\', '/') + '/target'
$vars['CARGO_INCREMENTAL'] = '1'

$runner = [Diagnostics.Process]::Start($start)
Write-Output "Started the runner (cmd $($runner.Id))."
