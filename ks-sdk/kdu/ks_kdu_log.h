/*******************************************************************************
*
*  TITLE:       KS_KDU_LOG.H
*
*  Force-included (/FI) into every KDU translation unit and ks_bridge.cpp
*  by ks-test's build.rs.
*
*  This header is injected BEFORE the source file's own includes, so it
*  pulls in <stdio.h>/<stdarg.h> first: their printf_s/vprintf_s
*  declarations are processed while no macro exists yet, and the include
*  guard keeps later <stdio.h> includes from re-declaring them under the
*  macros below. Every subsequent printf_s/vprintf_s call in the
*  translation unit is redirected to the embedding process log callback,
*  which ks-test prefixes with "kdu: " per line - same trace format the
*  old kdu.exe child process produced.
*
*  KsKduPrintf/KsKduVPrintf return the formatted length like the CRT
*  functions do: some KDU call sites use the return value.
*
*******************************************************************************/

#pragma once
#ifndef KS_KDU_LOG_H
#define KS_KDU_LOG_H

#include <stdio.h>
#include <stdarg.h>

typedef void(__cdecl* KsKduLogFn)(const char* line);

#ifdef __cplusplus
extern "C" {
#endif

void __cdecl KsKduSetLog(KsKduLogFn fn);
int __cdecl KsKduPrintf(const char* fmt, ...);
int __cdecl KsKduVPrintf(const char* fmt, va_list args);

#ifdef __cplusplus
}
#endif

#define printf_s(...) KsKduPrintf(__VA_ARGS__)
#define vprintf_s(fmt, args) KsKduVPrintf(fmt, args)

#endif // KS_KDU_LOG_H
