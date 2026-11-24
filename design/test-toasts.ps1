# Sends REAL Windows toasts (not Lumen's own --notify), so MacNotif's capture path is exercised.
# Uses Telegram / Discord / Brave app ids if installed, else Windows PowerShell's id.
# Run while Lumen is running with the MacNotif module on:  powershell -File design\test-toasts.ps1
[void][Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime]
[void][Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime]

$apps = Get-StartApps
function Find-App($pat) { ($apps | Where-Object Name -match $pat | Select-Object -First 1).AppID }
$fallback = '{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe'

# title, body, third line (Discord channel / Telegram account) -> shown next to the app name
$tests = @(
  @{ pat = 'Telegram'; t = 'Alice'; b = 'are we still on for tonight?'; x = 'account: @main' },
  @{ pat = 'Discord';  t = 'bob'; b = 'build is green';                  x = '#dev-chat, Lumen HQ' },
  @{ pat = 'Brave';    t = 'Inbox'; b = '3 new messages';                 x = $null }
)
foreach ($c in $tests) {
  $id = Find-App $c.pat
  if (-not $id) { $id = $fallback }
  $extra = if ($c.x) { "<text>$($c.x)</text>" } else { '' }
  $xml = New-Object Windows.Data.Xml.Dom.XmlDocument
  $xml.LoadXml("<toast><visual><binding template='ToastGeneric'><text>$($c.t)</text><text>$($c.b)</text>$extra</binding></visual></toast>")
  [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier($id).Show([Windows.UI.Notifications.ToastNotification]::new($xml))
  Write-Host "sent toast as $id"
  Start-Sleep -Milliseconds 900
}
