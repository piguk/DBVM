module tb;
  logic clk = 0, rst, push, pop; logic [7:0] din; wire [7:0] dout; wire full, empty; wire [3:0] count;
  fifo dut(.clk(clk), .rst(rst), .push(push), .pop(pop), .din(din), .dout(dout), .full(full), .empty(empty), .count(count));
  logic [15:0] r = 16'h7F11; logic [7:0] q[$];
  int pass = 0, total = 0;
  function automatic logic [15:0] step(input logic [15:0] s); return {s[14:0], s[15] ^ s[13] ^ s[12] ^ s[10]}; endfunction
  initial begin
    for (int i = 0; i < 600; i++) begin
      r = step(step(step(r)));
      rst = (i < 2) || (r[4:0] == 0); push = r[5] | r[6]; pop = (i % 200 < 100) ? r[7] & r[8] : r[7] | r[8]; din = r[15:8];
      #1 clk = 1;
      if (rst) q.delete();
      else begin
        logic do_push, do_pop; do_push = push && q.size() < 8; do_pop = pop && q.size() > 0;
        if (do_pop) void'(q.pop_front());
        if (do_push) q.push_back(din);
      end
      #1 clk = 0;
      total++;
      if (count === 4'(q.size()) && full === (q.size() == 8) && empty === (q.size() == 0) && (q.size() == 0 || dout === q[0])) pass++;
    end
    $display("RESULT %0d %0d", pass, total);
    $finish;
  end
endmodule
