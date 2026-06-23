// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

#include <stdio.h>
#include <stdlib.h>
#include <IOSurface/IOSurface.h>
int main(int argc,char**argv){
    uint32_t id=(uint32_t)atoi(argv[1]);
    IOSurfaceRef s=IOSurfaceLookup(id);
    printf("stranger IOSurfaceLookup(%u) -> %p  (%s)\n", id, (void*)s, s?"FOUND (exposed)":"NULL (hidden)");
    return 0;
}
