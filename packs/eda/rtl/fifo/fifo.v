// Synchronous FIFO, 8 entries of 8 bits. A push when full and a pop when empty are ignored; a push and a pop
// in the same cycle both happen. dout is the oldest entry; count the number held.
module fifo(input clk, input rst, input push, input pop, input [7:0] din, output [7:0] dout,
            output full, output empty, output reg [3:0] count);
  reg [7:0] mem [0:7];
  reg [2:0] rd, wr;
  wire do_push = push && !full;
  wire do_pop = pop && !empty;
  always @(posedge clk) begin
    if (rst) begin
      rd <= 3'd0; wr <= 3'd0; count <= 4'd0;
    end else begin
      if (do_push) begin mem[wr] <= din; wr <= wr + 3'd1; end
      if (do_pop) rd <= rd + 3'd1;
      count <= count + {3'd0, do_push} - {3'd0, do_pop};
    end
  end
  assign dout = mem[rd];
  assign full = (count == 4'd8);
  assign empty = (count == 4'd0);
endmodule
