obj-m := mmio_cost.o
KDIR ?= /lib/modules/$(shell uname -r)/build
all:
	$(MAKE) -C $(KDIR) M=$(CURDIR) modules
	cc -O2 -o bench bench.c
