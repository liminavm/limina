/* Decode the KK dispatch breadcrumb ring left behind by a crashed worker.
 *
 * The ring is a MAP_SHARED file written with plain stores before each compute copy, so it
 * survives a SIGSEGV that kills the process mid-encode. The entry whose `done` is still 0 is
 * the dispatch that was IN FLIGHT — i.e. the one that faulted.
 *
 * Build: cc -O2 -o dump dump.c
 * Use:   ./dump <logs>/kk-pool.txt.dispatch.<pid>
 */
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>

/* v2: entries carry the encoder generation and liveness. The magic was bumped with the layout so
 * an older dumper refuses the file rather than reading the fields at the wrong offsets. */
#define MAGIC 0x4c444d32u

struct entry {
   uint64_t seq, done, thread, encoder, buffer, texture;
   uint64_t offset_B, stride_B, image_2d_B;
   uint32_t w, h, d, x, y, z, slice, level, options, kind;
   uint64_t gen;
   uint32_t enc_state, pad2;
};

/* Mirrors enum limina_enc_state in mtl_encoder.m. */
static const char *encstate(uint32_t st)
{
   switch (st) {
   case 1: return "live";
   case 2: return "ENDED";
   case 3: return "RELEASED";
   default: return "unknown";
   }
}
struct hdr {
   uint32_t magic, entry_size, entries, pad;
   uint64_t next;
   struct entry e[];
};

static const char *kindname(uint32_t k)
{
   switch (k) {
   case 1: return "buf->img";
   case 2: return "img->buf";
   case 3: return "buf->buf";
   default: return "?";
   }
}

int main(int argc, char **argv)
{
   if (argc < 2) { fprintf(stderr, "usage: %s <dispatch-trace-file>\n", argv[0]); return 2; }
   FILE *f = fopen(argv[1], "rb");
   if (!f) { perror("open"); return 2; }
   fseek(f, 0, SEEK_END);
   long len = ftell(f);
   fseek(f, 0, SEEK_SET);
   void *buf = malloc((size_t)len);
   if (fread(buf, 1, (size_t)len, f) != (size_t)len) { perror("read"); return 2; }
   fclose(f);

   struct hdr *h = buf;
   if (h->magic != MAGIC) {
      fprintf(stderr, "bad magic 0x%x (expected 0x%x)\n", h->magic, MAGIC);
      return 2;
   }
   if (h->entry_size != sizeof(struct entry)) {
      fprintf(stderr, "entry size %u != %zu - dumper and driver disagree\n",
              h->entry_size, sizeof(struct entry));
      return 2;
   }
   printf("entries=%u written=%llu\n", h->entries, (unsigned long long)h->next);

   /* The in-flight ones first: that is the whole point of the file. */
   unsigned inflight = 0;
   for (unsigned i = 0; i < h->entries; i++) {
      struct entry *e = &h->e[i];
      if (e->seq && !e->done) {
         inflight++;
         printf("\n*** IN FLIGHT AT CRASH: seq=%llu thread=0x%llx %s\n",
                (unsigned long long)e->seq, (unsigned long long)e->thread, kindname(e->kind));
         printf("    size=%ux%ux%u origin=(%u,%u,%u) slice=%u level=%u options=0x%x\n",
                e->w, e->h, e->d, e->x, e->y, e->z, e->slice, e->level, e->options);
         printf("    offset=%llu stride=%llu image2d=%llu\n",
                (unsigned long long)e->offset_B, (unsigned long long)e->stride_B,
                (unsigned long long)e->image_2d_B);
         printf("    encoder=0x%llx (gen %llu, %s) buffer=0x%llx texture=0x%llx\n",
                (unsigned long long)e->encoder, (unsigned long long)e->gen,
                encstate(e->enc_state), (unsigned long long)e->buffer,
                (unsigned long long)e->texture);
      }
   }
   if (!inflight)
      printf("\n(no in-flight entry - the process did not die inside a copy)\n");

   /* Then the tail, newest last, so the fatal dispatch can be compared with its neighbours.
    * "Is this one unusual?" is the question the AGX fault actually turns on. */
   printf("\nlast 32 completed, oldest first:\n");
   uint64_t start = h->next > 32 ? h->next - 32 : 1;
   for (uint64_t s = start; s <= h->next; s++) {
      struct entry *e = &h->e[s % h->entries];
      if (e->seq != s) continue;
      printf("  seq=%-8llu %-9s %5ux%-5u lvl=%u stride=%-8llu enc=0x%llx gen=%-6llu %-8s %s\n",
             (unsigned long long)e->seq, kindname(e->kind), e->w, e->h, e->level,
             (unsigned long long)e->stride_B, (unsigned long long)e->encoder,
             (unsigned long long)e->gen, encstate(e->enc_state),
             e->done ? "" : "<- IN FLIGHT");
   }
   return 0;
}
