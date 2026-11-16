#ifndef CommonInstaller
  #error CommonInstaller is required
#endif

#include CommonInstaller

[Setup]
SignTool=developer
SignedUninstaller=yes
SignedUninstallerDir={#SignedOutput}
