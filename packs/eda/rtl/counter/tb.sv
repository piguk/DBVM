module tb;
  logic clk = 0, rst, en, up, load; logic [7:0] d; wire [7:0] q;
  counter dut(.clk(clk), .rst(rst), .en(en), .up(up), .load(load), .d(d), .q(q));
  logic [15:0] r = 16'h1D2B; logic [7:0] m;
  int pass = 0, total = 0;
  function automatic logic [15:0] step(input logic [15:0] s); return {s[14:0], s[15] ^ s[13] ^ s[12] ^ s[10]}; endfunction
  initial begin
    for (int i = 0; i < 400; i++) begin
      r = step(step(step(r)));
      rst = (i < 2) || (r[3:0] == 0); load = r[5:4] == 0; en = r[6]; up = r[7]; d = r[15:8];
      #1 clk = 1; #1 clk = 0;
      if (rst) m = 0; else if (load) m = d; else if (en) m = up ? m + 1 : m - 1;
      total++; if (q === m) pass++;
    end
    $display("RESULT %0d %0d", pass, total);
    $finish;
  end
endmodule
