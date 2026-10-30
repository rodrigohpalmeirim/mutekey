# Install VB-Cable (a virtual microphone) unattended, so a mic-less VM has a capture endpoint.
# Expects the extracted VBCABLE_Driver_Pack in $Dir.
param([string]$Dir = 'C:\mk\vbcable')

# Trust the driver's publisher first; otherwise Windows asks on the desktop and the install hangs.
$cert = (Get-AuthenticodeSignature "$Dir\vbaudio_cable64_win10.cat").SignerCertificate
$store = New-Object Security.Cryptography.X509Certificates.X509Store 'TrustedPublisher', 'LocalMachine'
$store.Open('ReadWrite'); $store.Add($cert); $store.Close()

Start-Process -Wait "$Dir\VBCABLE_Setup_x64.exe" -ArgumentList '-i', '-h'
Get-PnpDevice -Class AudioEndpoint | Format-Table -AutoSize Status, FriendlyName
