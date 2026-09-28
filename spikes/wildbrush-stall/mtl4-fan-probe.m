// Does the private "OpenGL mode" route to native triangle fans / primitive-restart control exist
// on Metal 4, the API KosmicKrisp uses? Upstream draft mesa!39602 drives it on the Metal 3 classes.
// Build: clang -fobjc-arc -framework Metal -framework Foundation mtl4-fan-probe.m -o mtl4-fan-probe
#import <Foundation/Foundation.h>
#import <Metal/Metal.h>
#import <objc/runtime.h>

static void dump(const char *name, NSArray<NSString *> *needles)
{
   Class c = objc_getClass(name);
   if (!c) {
      printf("%s: class not found\n", name);
      return;
   }
   for (Class k = c; k; k = class_getSuperclass(k)) {
      unsigned n = 0;
      Method *m = class_copyMethodList(k, &n);
      for (unsigned i = 0; i < n; i++) {
         NSString *sel = NSStringFromSelector(method_getName(m[i]));
         for (NSString *needle in needles) {
            if ([sel rangeOfString:needle options:NSCaseInsensitiveSearch].location != NSNotFound) {
               printf("%s (%s): %s\n", name, class_getName(k), sel.UTF8String);
               break;
            }
         }
      }
      free(m);
   }
}

int main(void)
{
   @autoreleasepool {
      id<MTLDevice> dev = MTLCreateSystemDefaultDevice();
      printf("device: %s\n", dev.name.UTF8String);
      NSArray *needles = @[ @"opengl", @"restart", @"fan" ];

      MTLRenderPassDescriptor *d3 = [MTLRenderPassDescriptor new];
      MTL4RenderPassDescriptor *d4 = [MTL4RenderPassDescriptor new];
      printf("MTL3 desc %s responds setOpenGLModeEnabled: %d\n", class_getName([d3 class]),
             [d3 respondsToSelector:@selector(setOpenGLModeEnabled:)]);
      printf("MTL4 desc %s responds setOpenGLModeEnabled: %d\n", class_getName([d4 class]),
             [d4 respondsToSelector:@selector(setOpenGLModeEnabled:)]);
      dump(class_getName([d3 class]), needles);
      dump(class_getName([d4 class]), needles);

      // The concrete encoder classes, as named in the worker's `sample` stacks.
      dump("AGXG13XFamilyRenderContext", needles);
      dump("AGXG13XFamilyRenderContext_mtlnext", needles);
      dump("AGXG13XFamilyCommandBuffer_mtlnext", needles);
   }
   return 0;
}
