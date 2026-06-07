// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

// Spike: can MoltenVK 1.4.1 give us an IOSurface-backed VkImage that the GPU renders
// into, and can that IOSurface be presented zero-copy via a CALayer?
//
// Gating experiment for the tier-2 zero-copy present architecture (roadmap M4, crossings
// B+D). vkr (virglrenderer's venus host side) links MoltenVK in-process exactly like this
// probe, so whatever works here, vkr can do for venus's scanout images.
//
// Two directions are tested:
//   EXPORT: create a normal VkImage with VkExportMetalObjectCreateInfoEXT(IOSURFACE) and ask
//           MoltenVK to hand back an IOSurface. (Informational — MoltenVK does NOT auto-create
//           one; see RESULTS.)
//   IMPORT: WE create the IOSurface -> MTLTexture, import it as the VkImage backing via
//           VkImportMetalTextureInfoEXT, render into it with Vulkan, then read/present OUR
//           IOSurface. This is the path vkr would use (vkr owns host image creation).
//
// MRC (no ARC): we read driver-returned id/CF out-params from C structs; process is short.

#define VK_USE_PLATFORM_METAL_EXT
#include <vulkan/vulkan.h>

#import <Foundation/Foundation.h>
#import <Metal/Metal.h>
#import <IOSurface/IOSurface.h>
#import <QuartzCore/QuartzCore.h>
#include <stdio.h>
#include <string.h>

#define W 256
#define H 256

#define CHECK(x) do { VkResult _r = (x); if (_r != VK_SUCCESS) { \
  fprintf(stderr, "FAIL %s -> %d (line %d)\n", #x, _r, __LINE__); return 2; } } while (0)

static int has_ext(VkExtensionProperties *p, uint32_t n, const char *name) {
  for (uint32_t i = 0; i < n; i++) if (!strcmp(p[i].extensionName, name)) return 1;
  return 0;
}

// Clear color (BGRA throughout so it's display-native and IOSurface byte order is obvious).
static const float CR=0.20f, CG=0.40f, CB=0.80f;
#define EB ((uint8_t)(CB*255+0.5f))   // 204
#define EG ((uint8_t)(CG*255+0.5f))   // 102
#define ER ((uint8_t)(CR*255+0.5f))   // 51

int main(void) {
@autoreleasepool {
  const char *instExts[] = { "VK_KHR_portability_enumeration" };
  VkApplicationInfo app = { .sType=VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion=VK_API_VERSION_1_2 };
  VkInstanceCreateInfo ici = { .sType=VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                               .flags=VK_INSTANCE_CREATE_ENUMERATE_PORTABILITY_BIT_KHR,
                               .pApplicationInfo=&app, .enabledExtensionCount=1,
                               .ppEnabledExtensionNames=instExts };
  VkInstance inst; CHECK(vkCreateInstance(&ici, NULL, &inst));
  uint32_t ndev=0; CHECK(vkEnumeratePhysicalDevices(inst,&ndev,NULL));
  if(!ndev){fprintf(stderr,"no devices\n");return 2;}
  VkPhysicalDevice phys[8]; if(ndev>8)ndev=8; CHECK(vkEnumeratePhysicalDevices(inst,&ndev,phys));
  VkPhysicalDevice gpu=phys[0];
  VkPhysicalDeviceProperties pprops; vkGetPhysicalDeviceProperties(gpu,&pprops);
  printf("== device: %s (Vulkan %u.%u.%u) ==\n", pprops.deviceName,
         VK_VERSION_MAJOR(pprops.apiVersion),VK_VERSION_MINOR(pprops.apiVersion),VK_VERSION_PATCH(pprops.apiVersion));

  uint32_t next=0; CHECK(vkEnumerateDeviceExtensionProperties(gpu,NULL,&next,NULL));
  VkExtensionProperties *ep=calloc(next,sizeof(*ep)); CHECK(vkEnumerateDeviceExtensionProperties(gpu,NULL,&next,ep));
  const char *survey[]={"VK_EXT_metal_objects","VK_EXT_external_memory_metal","VK_KHR_external_memory",
    "VK_KHR_external_memory_fd","VK_EXT_external_memory_dma_buf","VK_EXT_image_drm_format_modifier",
    "VK_EXT_queue_family_foreign","VK_KHR_portability_subset"};
  printf("== device extension survey ==\n");
  for(size_t i=0;i<sizeof(survey)/sizeof(*survey);i++)
    printf("  [%s] %s\n", has_ext(ep,next,survey[i])?"YES":" no", survey[i]);
  if(!has_ext(ep,next,"VK_EXT_metal_objects")){printf("\nRESULT: VK_EXT_metal_objects ABSENT\n");return 1;}

  uint32_t nqf=0; vkGetPhysicalDeviceQueueFamilyProperties(gpu,&nqf,NULL);
  VkQueueFamilyProperties qf[16]; if(nqf>16)nqf=16; vkGetPhysicalDeviceQueueFamilyProperties(gpu,&nqf,qf);
  uint32_t qfi=0; for(uint32_t i=0;i<nqf;i++) if(qf[i].queueFlags&VK_QUEUE_GRAPHICS_BIT){qfi=i;break;}
  float prio=1.0f;
  VkDeviceQueueCreateInfo qci={.sType=VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,.queueFamilyIndex=qfi,.queueCount=1,.pQueuePriorities=&prio};
  const char *devExts[]={"VK_EXT_metal_objects","VK_KHR_portability_subset"};
  uint32_t ndevExt=has_ext(ep,next,"VK_KHR_portability_subset")?2:1;
  VkDeviceCreateInfo dci={.sType=VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,.queueCreateInfoCount=1,.pQueueCreateInfos=&qci,
                          .enabledExtensionCount=ndevExt,.ppEnabledExtensionNames=devExts};
  VkDevice dev; CHECK(vkCreateDevice(gpu,&dci,NULL,&dev));
  VkQueue queue; vkGetDeviceQueue(dev,qfi,0,&queue);

  id<MTLDevice> mdev = MTLCreateSystemDefaultDevice();

  // ============================================================================
  // EXPORT direction (informational): does MoltenVK auto-create an IOSurface from
  // VkExportMetalObjectCreateInfoEXT(IOSURFACE)? (Spoiler from prior runs: no.)
  // ============================================================================
  {
    VkExportMetalObjectCreateInfoEXT eTex={.sType=VK_STRUCTURE_TYPE_EXPORT_METAL_OBJECT_CREATE_INFO_EXT,
      .exportObjectType=VK_EXPORT_METAL_OBJECT_TYPE_METAL_TEXTURE_BIT_EXT};
    VkExportMetalObjectCreateInfoEXT eIO={.sType=VK_STRUCTURE_TYPE_EXPORT_METAL_OBJECT_CREATE_INFO_EXT,
      .pNext=&eTex,.exportObjectType=VK_EXPORT_METAL_OBJECT_TYPE_METAL_IOSURFACE_BIT_EXT};
    VkImageCreateInfo ic={.sType=VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,.pNext=&eIO,.imageType=VK_IMAGE_TYPE_2D,
      .format=VK_FORMAT_B8G8R8A8_UNORM,.extent={W,H,1},.mipLevels=1,.arrayLayers=1,.samples=VK_SAMPLE_COUNT_1_BIT,
      .tiling=VK_IMAGE_TILING_OPTIMAL,.usage=VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT|VK_IMAGE_USAGE_TRANSFER_DST_BIT,
      .sharingMode=VK_SHARING_MODE_EXCLUSIVE,.initialLayout=VK_IMAGE_LAYOUT_UNDEFINED};
    VkImage im; if(vkCreateImage(dev,&ic,NULL,&im)==VK_SUCCESS){
      VkMemoryRequirements mr; vkGetImageMemoryRequirements(dev,im,&mr);
      VkPhysicalDeviceMemoryProperties mp; vkGetPhysicalDeviceMemoryProperties(gpu,&mp);
      uint32_t mti=0; for(uint32_t i=0;i<mp.memoryTypeCount;i++) if(mr.memoryTypeBits&(1u<<i)){mti=i;break;}
      VkExportMetalObjectCreateInfoEXT mIO=eIO; mIO.pNext=NULL;
      VkMemoryAllocateInfo ma={.sType=VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,.pNext=&mIO,.allocationSize=mr.size,.memoryTypeIndex=mti};
      VkDeviceMemory mm; if(vkAllocateMemory(dev,&ma,NULL,&mm)==VK_SUCCESS && vkBindImageMemory(dev,im,mm,0)==VK_SUCCESS){
        VkExportMetalIOSurfaceInfoEXT oIO={.sType=VK_STRUCTURE_TYPE_EXPORT_METAL_IO_SURFACE_INFO_EXT,.image=im};
        VkExportMetalObjectsInfoEXT objs={.sType=VK_STRUCTURE_TYPE_EXPORT_METAL_OBJECTS_INFO_EXT,.pNext=&oIO};
        ((PFN_vkExportMetalObjectsEXT)vkGetDeviceProcAddr(dev,"vkExportMetalObjectsEXT"))(dev,&objs);
        printf("\n== EXPORT direction: MoltenVK auto-create IOSurface -> %s ==\n",
               oIO.ioSurface? "YES" : "NO (returns NULL; must import our own — see below)");
      }
    }
  }

  // ============================================================================
  // IMPORT direction (the path vkr uses): WE own the IOSurface, render into it
  // through Vulkan, then present it.
  // ============================================================================
  printf("\n== IMPORT direction: our IOSurface -> MTLTexture -> VkImage -> GPU render -> present ==\n");

  // 1. Create the IOSurface (BGRA8, display-native).
  NSDictionary *props = @{ (id)kIOSurfaceWidth:@(W), (id)kIOSurfaceHeight:@(H),
                           (id)kIOSurfaceBytesPerElement:@4, (id)kIOSurfacePixelFormat:@((uint32_t)'BGRA') };
  IOSurfaceRef io = IOSurfaceCreate((CFDictionaryRef)props);
  if(!io){printf("  IOSurfaceCreate FAILED\n");return 2;}
  printf("  IOSurface %zux%zu bpr=%zu alloc=%zu\n",IOSurfaceGetWidth(io),IOSurfaceGetHeight(io),
         IOSurfaceGetBytesPerRow(io),IOSurfaceGetAllocSize(io));

  // 2. Wrap it in an MTLTexture (IOSurface-backed; this is the zero-copy GPU view).
  MTLTextureDescriptor *td=[MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatBGRA8Unorm width:W height:H mipmapped:NO];
  td.usage=MTLTextureUsageShaderRead|MTLTextureUsageRenderTarget;
  td.storageMode=MTLStorageModeShared;
  id<MTLTexture> ioTex=[mdev newTextureWithDescriptor:td iosurface:io plane:0];
  if(!ioTex){printf("  newTextureWithDescriptor:iosurface FAILED\n");return 2;}
  printf("  MTLTexture from IOSurface: OK (iosurface=%p)\n",(void*)[ioTex iosurface]);

  // 3. Import that MTLTexture as a VkImage's backing.
  VkImportMetalTextureInfoEXT imp={.sType=VK_STRUCTURE_TYPE_IMPORT_METAL_TEXTURE_INFO_EXT,
                                   .plane=VK_IMAGE_ASPECT_COLOR_BIT,.mtlTexture=ioTex};
  VkImageCreateInfo ic={.sType=VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,.pNext=&imp,.imageType=VK_IMAGE_TYPE_2D,
    .format=VK_FORMAT_B8G8R8A8_UNORM,.extent={W,H,1},.mipLevels=1,.arrayLayers=1,.samples=VK_SAMPLE_COUNT_1_BIT,
    .tiling=VK_IMAGE_TILING_OPTIMAL,.usage=VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT|VK_IMAGE_USAGE_TRANSFER_DST_BIT|VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
    .sharingMode=VK_SHARING_MODE_EXCLUSIVE,.initialLayout=VK_IMAGE_LAYOUT_UNDEFINED};
  VkImage image; CHECK(vkCreateImage(dev,&ic,NULL,&image));
  // Imported-texture images still need a (placeholder) memory binding in MoltenVK.
  VkMemoryRequirements mr; vkGetImageMemoryRequirements(dev,image,&mr);
  printf("  VkImage(import) memreq size=%llu bits=0x%x\n",(unsigned long long)mr.size,mr.memoryTypeBits);
  if(mr.size>0){
    VkPhysicalDeviceMemoryProperties mp; vkGetPhysicalDeviceMemoryProperties(gpu,&mp);
    uint32_t mti=0; for(uint32_t i=0;i<mp.memoryTypeCount;i++) if(mr.memoryTypeBits&(1u<<i)){mti=i;break;}
    VkMemoryAllocateInfo ma={.sType=VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,.allocationSize=mr.size,.memoryTypeIndex=mti};
    VkDeviceMemory mm; CHECK(vkAllocateMemory(dev,&ma,NULL,&mm));
    CHECK(vkBindImageMemory(dev,image,mm,0));
    printf("  bound placeholder memory (type %u)\n",mti);
  }

  // 4. GPU clears the VkImage (== writes through to the IOSurface).
  VkCommandPool pool; VkCommandPoolCreateInfo pci={.sType=VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,.queueFamilyIndex=qfi};
  CHECK(vkCreateCommandPool(dev,&pci,NULL,&pool));
  VkCommandBuffer cb; VkCommandBufferAllocateInfo cbai={.sType=VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,.commandPool=pool,.level=VK_COMMAND_BUFFER_LEVEL_PRIMARY,.commandBufferCount=1};
  CHECK(vkAllocateCommandBuffers(dev,&cbai,&cb));
  VkCommandBufferBeginInfo bi={.sType=VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,.flags=VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};
  CHECK(vkBeginCommandBuffer(cb,&bi));
  VkImageMemoryBarrier b={.sType=VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,.oldLayout=VK_IMAGE_LAYOUT_UNDEFINED,
    .newLayout=VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,.srcQueueFamilyIndex=VK_QUEUE_FAMILY_IGNORED,.dstQueueFamilyIndex=VK_QUEUE_FAMILY_IGNORED,
    .image=image,.subresourceRange={VK_IMAGE_ASPECT_COLOR_BIT,0,1,0,1},.srcAccessMask=0,.dstAccessMask=VK_ACCESS_TRANSFER_WRITE_BIT};
  vkCmdPipelineBarrier(cb,VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,VK_PIPELINE_STAGE_TRANSFER_BIT,0,0,NULL,0,NULL,1,&b);
  VkClearColorValue clear={.float32={CR,CG,CB,1.0f}};
  VkImageSubresourceRange rng={VK_IMAGE_ASPECT_COLOR_BIT,0,1,0,1};
  vkCmdClearColorImage(cb,image,VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,&clear,1,&rng);
  VkImageMemoryBarrier b2=b; b2.oldLayout=VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL; b2.newLayout=VK_IMAGE_LAYOUT_GENERAL;
  b2.srcAccessMask=VK_ACCESS_TRANSFER_WRITE_BIT; b2.dstAccessMask=VK_ACCESS_HOST_READ_BIT;
  vkCmdPipelineBarrier(cb,VK_PIPELINE_STAGE_TRANSFER_BIT,VK_PIPELINE_STAGE_HOST_BIT,0,0,NULL,0,NULL,1,&b2);
  CHECK(vkEndCommandBuffer(cb));
  VkSubmitInfo si={.sType=VK_STRUCTURE_TYPE_SUBMIT_INFO,.commandBufferCount=1,.pCommandBuffers=&cb};
  CHECK(vkQueueSubmit(queue,1,&si,VK_NULL_HANDLE)); CHECK(vkQueueWaitIdle(queue));
  printf("  GPU cleared VkImage to BGRA(%u,%u,%u,255)\n",EB,EG,ER);

  // 5. Read back through OUR IOSurface (zero-copy host read of the GPU's write).
  IOSurfaceLock(io,kIOSurfaceLockReadOnly,NULL);
  uint8_t *base=(uint8_t*)IOSurfaceGetBaseAddress(io);
  uint8_t p0=base[0],p1=base[1],p2=base[2],p3=base[3];
  IOSurfaceUnlock(io,kIOSurfaceLockReadOnly,NULL);
  int io_ok = (abs(p0-EB)<=2 && abs(p1-EG)<=2 && abs(p2-ER)<=2);
  printf("  IOSurface base[0..3]=BGRA(%u,%u,%u,%u) %s\n",p0,p1,p2,p3, io_ok? "<-- MATCH (GPU render seen in IOSurface, zero-copy)":"(MISMATCH)");

  // 6. Present feasibility: IOSurface -> CALayer.contents, and a fresh MTLTexture wrap.
  int present_ok=0;
  @try {
    CALayer *layer=[CALayer layer]; layer.frame=CGRectMake(0,0,W,H); layer.contents=(id)io;
    id<MTLTexture> wrap=[mdev newTextureWithDescriptor:td iosurface:io plane:0];
    present_ok = (layer.contents!=nil) && (wrap!=nil);
    printf("  CALayer.contents=IOSurface: %s ; re-wrap as MTLTexture: %s\n",
           layer.contents?"accepted":"REJECTED", wrap?"OK":"nil");
  } @catch(NSException *e){ printf("  present FAILED: %s\n",[[e reason] UTF8String]); }

  printf("\n==== VERDICT (IMPORT path = what vkr would do) ====\n");
  printf("  VK_EXT_metal_objects        : YES\n");
  printf("  IOSurface->MTLTexture->VkImage import + GPU render : %s\n", io_ok?"YES":"NO");
  printf("  IOSurface readable zero-copy (host)               : %s\n", io_ok?"YES":"NO");
  printf("  IOSurface -> CALayer present                       : %s\n", present_ok?"YES":"NO");
  printf("\n  => zero-copy present currency (B+D) via IMPORT is %s.\n",
         (io_ok&&present_ok)?"VIABLE":"NOT viable");
  return (io_ok&&present_ok)?0:1;
}
}
