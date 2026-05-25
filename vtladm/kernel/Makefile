KDIR ?= /lib/modules/$(shell uname -r)/build
PWD := $(shell pwd)

all:
	$(MAKE) -C $(KDIR) M=$(PWD) modules

clean:
	$(MAKE) -C $(KDIR) M=$(PWD) clean
	rm -f *.ko *.o *.mod *.mod.c modules.order Module.symvers .*.cmd
	rm -f src/*.o src/*.mod src/.*.cmd

install:
	$(MAKE) -C $(KDIR) M=$(PWD) modules_install
	depmod -a
