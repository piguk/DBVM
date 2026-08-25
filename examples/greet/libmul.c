// for preload demo: interpose greet() to show LD_PRELOAD-as-a-row
#include <stdio.h>
void greet(void){ printf("Hello, world, traced via preload!\n"); }
