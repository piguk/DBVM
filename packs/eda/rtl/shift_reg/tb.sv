module tb;
  logic clk = 0, load, sin; logic [7:0] d; logic [1:0] mode; wire [7:0] q;
  shift_reg dut(.clk(clk), .load(load), .d(d), .mode(mode), .sin(sin), .q(q));
  logic [15:0] r = 16'h9999; logic [7:0] m;
  int pass = 0, total = 0;
  function automatic logic [15:0] step(input logic [15:0] s); return {s[14:0], s[15] ^ s[13] ^ s[12] ^ s[10]}; endfunction
  initial begin
    for (int i = 0; i < 400; i++) begin
      r = step(step(step(r))); load = (i == 0) || (r[4:0] == 0); d = r[15:8]; mode = r[6:5]; sin = r[7];
      #1 clk = 1; #1 clk = 0;
      if (load) m = d; else if (mode == 1) m = {m[6:0], sin}; else if (mode == 2) m = {sin, m[7:1]};
      total++; if (q === m) pass++;
    end
    $display("RESULT %0d %0d", pass, total);
    $finish;
  end
endmodule
