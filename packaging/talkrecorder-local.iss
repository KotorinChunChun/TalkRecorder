#ifndef Payload
  #error Payload is required
#endif
#ifndef OutputDirectory
  #error OutputDirectory is required
#endif
#ifndef ToolVersion
  #error ToolVersion is required
#endif

[Setup]
AppId=KotorinToys.TalkRecorder
AppName=TalkRecorder
AppVersion={#ToolVersion}
DefaultDirName={localappdata}\Programs\TalkRecorder
PrivilegesRequired=lowest
ArchitecturesAllowed=x64os
ArchitecturesInstallIn64BitMode=x64os
MinVersion=10.0.22000
OutputDir={#OutputDirectory}
OutputBaseFilename=TalkRecorder-{#ToolVersion}-setup
Compression=lzma2/fast
SolidCompression=yes
CloseApplications=no
RestartApplications=no
DisableProgramGroupPage=yes
UninstallDisplayIcon={app}\TalkRecorder.exe

[Languages]
Name: "japanese"; MessagesFile: "compiler:Languages\Japanese.isl"

[Files]
Source: "{#Payload}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{userprograms}\TalkRecorder"; Filename: "{app}\TalkRecorder.exe"
