!ifndef APP_EXE
  !error "Pass -DAPP_EXE=path/to/memory-forge.exe"
!endif
!ifndef OUTPUT_EXE
  !error "Pass -DOUTPUT_EXE=path/to/installer.exe"
!endif
!ifndef APP_VERSION
  !define APP_VERSION "3.5.0"
!endif

!include "MUI2.nsh"

!define APP_NAME "Memory Forge"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\com.voidcraft.memoryforge"

Name "${APP_NAME} ${APP_VERSION}"
OutFile "${OUTPUT_EXE}"
InstallDir "$LOCALAPPDATA\Programs\Memory Forge"
InstallDirRegKey HKCU "${UNINSTALL_KEY}" "InstallLocation"
RequestExecutionLevel user
Unicode true
SetCompressor /SOLID lzma
ShowInstDetails show
ShowUninstDetails show

!define MUI_ICON "memory-forge-installer.ico"
!define MUI_UNICON "memory-forge-installer.ico"
!define MUI_FINISHPAGE_RUN "$INSTDIR\memory-forge.exe"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "SimpChinese"
!insertmacro MUI_LANGUAGE "English"

Section "Install"
  SetOutPath "$INSTDIR"
  SetOverwrite on
  File "/oname=memory-forge.exe" "${APP_EXE}"
  WriteUninstaller "$INSTDIR\Uninstall.exe"

  CreateDirectory "$SMPROGRAMS\Memory Forge"
  CreateShortCut "$SMPROGRAMS\Memory Forge\Memory Forge.lnk" "$INSTDIR\memory-forge.exe"
  CreateShortCut "$SMPROGRAMS\Memory Forge\Uninstall.lnk" "$INSTDIR\Uninstall.exe"

  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "${APP_NAME}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${APP_VERSION}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "VoidCraft"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\memory-forge.exe"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1
SectionEnd

Section "Uninstall"
  Delete "$SMPROGRAMS\Memory Forge\Memory Forge.lnk"
  Delete "$SMPROGRAMS\Memory Forge\Uninstall.lnk"
  RMDir "$SMPROGRAMS\Memory Forge"
  Delete "$INSTDIR\memory-forge.exe"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"
  DeleteRegKey HKCU "${UNINSTALL_KEY}"
SectionEnd
