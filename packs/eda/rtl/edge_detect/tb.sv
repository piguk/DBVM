module tb;
  logic clk = 0, rst, din; wire rise, fall;
  edge_detect dut(.clk(clk), .rst(rst), .din(din), .rise(rise), .fall(fall));
  logic [15:0] r = 16'h2468; logic prev, mr, mf;
  int pass = 0, total = 0;
  function automatic logic [15:0] step(input logic [15:0] s); return {s[14:0], s[15] ^ s[13] ^ s[12] ^ s[10]}; endfunction
  initial begin
    for (int i = 0; i < 400; i++) begin
      r = step(step(r)); rst = (i < 2) || (r[5:0] == 0); din = (i % 50 < 25) ? r[6] : r[6] & r[7];
      #1 clk = 1; #1 clk = 0;
      if (rst) begin prev = 0; mr = 0; mf = 0; end else begin mr = din & ~prev; mf = ~din & prev; prev = din; end
      total++; if (rise === mr && fall === mf) pass++;
    end
    $display("RESULT %0d %0d", pass, total);
    $finish;
  end
endmodule
