Unicode true
ManifestDPIAware true
RequestExecutionLevel user
SetCompressor /SOLID lzma
CRCCheck force

!include "MUI2.nsh"
!include "FileFunc.nsh"
!include "${FILES_INCLUDE}"

Var LocalTranscriptAppExecutable

Name "SayTrace Runtime (${RUNTIME_VARIANT})"
OutFile "${OUTPUT_FILE}"
InstallDir "$LOCALAPPDATA\com.localtranscript.desktop\runtime"
InstallDirRegKey HKCU "Software\SayTrace" "RuntimeDirectory"

VIProductVersion "${VERSION_QUAD}"
VIAddVersionKey /LANG=1033 "ProductName" "SayTrace Runtime"
VIAddVersionKey /LANG=1033 "CompanyName" "SayTrace"
VIAddVersionKey /LANG=1033 "FileDescription" "SayTrace ${RUNTIME_VARIANT} runtime pack"
VIAddVersionKey /LANG=1033 "FileVersion" "${RUNTIME_VERSION}"
VIAddVersionKey /LANG=1033 "ProductVersion" "${RUNTIME_VERSION}"
VIAddVersionKey /LANG=1033 "LegalCopyright" "Copyright 2026 acamporat and SayTrace contributors"

!define MUI_ABORTWARNING
!define MUI_ICON "${INSTALLER_ICON}"
!define MUI_UNICON "${INSTALLER_ICON}"
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

!macro EnsureLocalTranscriptIdle LABEL ACTION
  nsExec::ExecToStack '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoLogo -NoProfile -NonInteractive -Command "if (Get-Process -Name local-transcript,local-transcript-worker -ErrorAction SilentlyContinue) { exit 23 }"'
  Pop $0
  Pop $1
  StrCmp $0 "0" ${LABEL}_clear
  StrCmp $0 "23" ${LABEL}_running
    MessageBox MB_ICONSTOP|MB_OK "Setup could not verify that SayTrace is closed. Close the application and its local worker, then try again."
    Abort
  ${LABEL}_running:
    MessageBox MB_ICONSTOP|MB_OK "Close SayTrace and wait for recording and processing to stop before ${ACTION} this runtime pack."
    Abort
  ${LABEL}_clear:
!macroend

!macro WaitForLocalTranscriptIdle LABEL ACTION
  StrCpy $2 0
  ${LABEL}_check:
    nsExec::ExecToStack '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoLogo -NoProfile -NonInteractive -Command "if (Get-Process -Name local-transcript,local-transcript-worker -ErrorAction SilentlyContinue) { exit 23 }"'
    Pop $0
    Pop $1
    StrCmp $0 "0" ${LABEL}_clear
    StrCmp $0 "23" ${LABEL}_retry
      MessageBox MB_ICONSTOP|MB_OK "Setup could not verify that SayTrace is closed. Close the application and its local worker, then try again."
      Abort
    ${LABEL}_retry:
      IntCmp $2 40 ${LABEL}_running ${LABEL}_wait ${LABEL}_running
    ${LABEL}_wait:
      IntOp $2 $2 + 1
      Sleep 500
      Goto ${LABEL}_check
    ${LABEL}_running:
      MessageBox MB_ICONSTOP|MB_OK "Close SayTrace and wait for recording and processing to stop before ${ACTION} this runtime pack."
      Abort
    ${LABEL}_clear:
!macroend

Function .onInit
  SetShellVarContext current
  ${GetParameters} $R0
  ${GetOptions} $R0 "/LOCALTRANSCRIPT_HANDOFF=" $R1
  ${GetOptions} $R0 "/LOCALTRANSCRIPT_APP_EXE=" $LocalTranscriptAppExecutable
  StrCmp $R1 "1" runtime_install_handoff runtime_install_direct
  runtime_install_handoff:
    !insertmacro WaitForLocalTranscriptIdle runtime_install_wait "installing"
    Goto runtime_install_ready
  runtime_install_direct:
    !insertmacro EnsureLocalTranscriptIdle runtime_install "installing"
  runtime_install_ready:
FunctionEnd

Function .onInstSuccess
  StrCmp $LocalTranscriptAppExecutable "" runtime_restart_done
  IfFileExists "$LocalTranscriptAppExecutable" 0 runtime_restart_done
  ${GetFileName} "$LocalTranscriptAppExecutable" $R0
  StrCmp $R0 "local-transcript.exe" 0 runtime_restart_done
  ExecShell "open" "$LocalTranscriptAppExecutable"
  runtime_restart_done:
FunctionEnd

Function un.onInit
  SetShellVarContext current
  !insertmacro EnsureLocalTranscriptIdle runtime_uninstall "uninstalling"
FunctionEnd

Section "SayTrace Runtime" SEC_RUNTIME
  SetShellVarContext current
  SetOverwrite on
  !insertmacro InstallRuntimePayload

  WriteUninstaller "$INSTDIR\uninstall-local-transcript-runtime.exe"
  WriteRegStr HKCU "Software\SayTrace" "RuntimeDirectory" "$INSTDIR"
  WriteRegStr HKCU "Software\SayTrace" "RuntimeVariant" "${RUNTIME_VARIANT}"
  WriteRegStr HKCU "Software\SayTrace" "RuntimeVersion" "${RUNTIME_VERSION}"

  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\SayTrace Runtime" "DisplayName" "SayTrace Runtime (${RUNTIME_VARIANT})"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\SayTrace Runtime" "DisplayVersion" "${RUNTIME_VERSION}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\SayTrace Runtime" "Publisher" "SayTrace"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\SayTrace Runtime" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\SayTrace Runtime" "UninstallString" '"$INSTDIR\uninstall-local-transcript-runtime.exe"'
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\SayTrace Runtime" "NoModify" 1
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\SayTrace Runtime" "NoRepair" 1
SectionEnd

Section "Uninstall"
  SetShellVarContext current
  !insertmacro UninstallRuntimePayload
  Delete "$INSTDIR\uninstall-local-transcript-runtime.exe"
  RMDir "$INSTDIR"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\SayTrace Runtime"
  DeleteRegValue HKCU "Software\SayTrace" "RuntimeDirectory"
  DeleteRegValue HKCU "Software\SayTrace" "RuntimeVariant"
  DeleteRegValue HKCU "Software\SayTrace" "RuntimeVersion"
  DeleteRegKey /ifempty HKCU "Software\SayTrace"
SectionEnd
