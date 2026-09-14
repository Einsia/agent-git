import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'

export function protectWindowsSmokeHome(home) {
  if (process.platform !== 'win32') return
  const protect = spawnSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', `
    $ErrorActionPreference = 'Stop'
    $sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
    $acl = New-Object System.Security.AccessControl.DirectorySecurity
    $acl.SetOwner($sid)
    $acl.SetAccessRuleProtection($true, $false)
    $rule = New-Object System.Security.AccessControl.FileSystemAccessRule($sid, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow')
    $acl.AddAccessRule($rule)
    [System.IO.Directory]::SetAccessControl($env:AGIT_WINDOWS_SMOKE_HOME, $acl)
  `], { encoding: 'utf8', env: { ...process.env, AGIT_WINDOWS_SMOKE_HOME: home }, timeout: 10000 })
  assert.equal(protect.status, 0, `owned Windows smoke home needs a private ACL: ${protect.error || protect.stderr}`)
}
