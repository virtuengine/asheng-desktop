; 语音模块安装钩子
;
; 初始安装包不含语音资源（asr-host + 模型合计约 300MB，压缩后约 170MB），
; 主体装完后在这里下载，把下载、校验、解压都收进安装流程。

!define VOICE_MANIFEST "https://43.142.55.26:8668/dl/desktop/voice-pack.json"

!macro NSIS_HOOK_POSTINSTALL
  ; 已装过的（覆盖安装/升级）不重复下载
  IfFileExists "$INSTDIR\asr\.ok" voice_done
  ; 静默安装不下载，避免无人值守时卡在这里
  IfSilent voice_done

  StrCpy $R0 "$INSTDIR\nsis\voice-install.ps1"
  ${IfNot} ${FileExists} "$R0"
    StrCpy $R0 "$INSTDIR\voice-install.ps1"
  ${EndIf}
  ${IfNot} ${FileExists} "$R0"
    DetailPrint "未找到语音安装脚本，跳过语音模块"
    Goto voice_done
  ${EndIf}

  voice_retry:
  DetailPrint "正在下载语音模块…"
  ExecWait '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File "$R0" -Dest "$INSTDIR\asr" -Manifest "${VOICE_MANIFEST}"' $R1
  ${If} $R1 == 0
    DetailPrint "语音模块安装完成"
    Goto voice_done
  ${EndIf}
  ${If} $R1 == 7
    DetailPrint "已跳过语音模块"
    Goto voice_done
  ${EndIf}

  MessageBox MB_ICONEXCLAMATION|MB_RETRYCANCEL "语音模块安装失败（错误码 $R1）。$\n$\n请确认网络正常后重试；也可以先跳过，不影响其他功能，之后重新运行安装包即可再装。" IDRETRY voice_retry

  voice_done:
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; 语音资源是安装后下载的，卸载器不会自动清理
  RMDir /r "$INSTDIR\asr"
  Delete "$INSTDIR\nsis\voice-install.ps1"
!macroend
