// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

#include <stdio.h>
#include <unistd.h>
#include <IOSurface/IOSurface.h>
#include <CoreFoundation/CoreFoundation.h>
static CFNumberRef num(int v){ return CFNumberCreate(NULL,kCFNumberIntType,&v); }
int main(int argc,char**argv){
    int global = argc>1 && argv[1][0]=='g';
    int w=64,h=8;
    const void*keys[]={kIOSurfaceWidth,kIOSurfaceHeight,kIOSurfaceBytesPerElement,kIOSurfaceBytesPerRow,kIOSurfacePixelFormat,kIOSurfaceIsGlobal};
    const void*vals[]={num(w),num(h),num(4),num(w*4),num('BGRA'),global?kCFBooleanTrue:kCFBooleanFalse};
    CFDictionaryRef d=CFDictionaryCreate(NULL,keys,vals,global?6:5,&kCFTypeDictionaryKeyCallBacks,&kCFTypeDictionaryValueCallBacks);
    IOSurfaceRef s=IOSurfaceCreate(d);
    printf("%u\n", IOSurfaceGetID(s)); fflush(stdout);
    sleep(20); return 0;
}
